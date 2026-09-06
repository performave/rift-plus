//! Installing, loading and removing rift's scripting addition.
//!
//! Ported from yabai's `src/sa.m` (MIT; see `src/osax/LICENSE-yabai`), with
//! rift's own bundle path, socket name and sudoers rule, so a machine running
//! rift needs nothing of yabai's installed.
//!
//! What the pieces are, in the order they are used:
//!
//! - `src/osax/payload.m` is compiled into a bundle that is `dlopen`ed *inside
//!   Dock*, where it serves `/tmp/rift-sa_$USER.socket`. Everything in
//!   [`crate::sys::scripting_addition`] is a client of that socket.
//! - `src/osax/loader.m` is a small executable that allocates a stack and some
//!   shellcode in Dock's task, spawns a thread on it and lets it `dlopen` the
//!   payload. It is what actually performs the injection.
//! - This module writes both into `/Library/ScriptingAdditions/rift.osax`,
//!   normalizes the loader's arm64e PAC ABI to match the running Dock,
//!   ad-hoc signs them, and runs the loader.
//!
//! Injecting into Dock is only possible with SIP's filesystem protections and
//! debugging restrictions disabled, and on Apple Silicon with the
//! `-arm64e_preview_abi` boot-arg set; both are checked before anything is
//! attempted, and reported rather than worked around.
//!
//! The payload belongs to Dock once loaded, not to rift: it survives rift
//! exiting, and a Dock restart drops it until `rift sa load` runs again.

use std::fs::{self, File};
use std::io;
use std::os::unix::fs::{FileExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use clap::Subcommand;
use nix::unistd::{Uid, User, getuid};

use crate::sys::scripting_addition;

/// The payload version this build ships, mirroring `OSAX_VERSION` in
/// `src/osax/common.h`. A payload already inside Dock that answers with
/// anything else is treated as stale.
pub const OSAX_VERSION: &str = "1.3.3";

const OSAX_BASE_DIR: &str = "/Library/ScriptingAdditions/rift.osax";
const SUDOERS_PATH: &str = "/private/etc/sudoers.d/rift";
const SUDOERS_TMP_PATH: &str = "/private/etc/sudoers.d/rift.tmp";
const DOCK_BINARY: &str = "/System/Library/CoreServices/Dock.app/Contents/MacOS/Dock";

const LOADER_BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/loader"));
const PAYLOAD_BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/payload"));

#[derive(Subcommand)]
pub enum SaCommands {
    /// Report whether the payload inside Dock is loaded and healthy
    Status,
    /// Install the scripting addition and inject it into Dock (requires root)
    Load,
    /// Write the scripting addition to disk without injecting (requires root)
    Install,
    /// Remove the scripting addition from disk (requires root)
    Uninstall {
        /// Also remove the sudoers rule: everything `rift sa` put outside
        /// Homebrew's prefix, which `brew uninstall` cannot reach
        #[arg(long)]
        all: bool,
    },
    /// Allow passwordless `sudo rift sa load` for the invoking user
    InstallSudoers,
    /// Remove the passwordless `sudo rift sa load` rule
    UninstallSudoers,
}

pub fn handle_sa_command(cmd: &SaCommands) -> Result<String, String> {
    match cmd {
        SaCommands::Status => status(),
        SaCommands::Load => load(),
        SaCommands::Install => {
            require_root("installed")?;
            require_sip_friendly()?;
            install()?;
            Ok(format!(
                "scripting addition v{OSAX_VERSION} installed at {OSAX_BASE_DIR}"
            ))
        }
        SaCommands::Uninstall { all } => {
            require_root("uninstalled")?;
            let mut report = if is_installed() {
                remove().map_err(|error| format!("failed to remove {OSAX_BASE_DIR}: {error}"))?;
                format!("removed {OSAX_BASE_DIR}")
            } else {
                format!("no scripting addition installed at {OSAX_BASE_DIR}")
            };
            if *all {
                report.push('\n');
                report.push_str(&uninstall_sudoers()?);
                // Nothing here unloads the payload: it belongs to Dock once
                // injected, and only Dock going away takes it with it.
                report.push_str(
                    "\na payload already inside Dock stays until Dock restarts ('killall Dock')",
                );
            }
            Ok(report)
        }
        SaCommands::InstallSudoers => install_sudoers(),
        SaCommands::UninstallSudoers => uninstall_sudoers(),
    }
}

/// Where each piece of the bundle lives. yabai builds these paths as strings
/// once into globals; here they are cheap enough to derive on demand.
struct Paths;

impl Paths {
    fn base() -> PathBuf { PathBuf::from(OSAX_BASE_DIR) }

    fn contents() -> PathBuf { Self::base().join("Contents") }

    fn info_plist() -> PathBuf { Self::contents().join("Info.plist") }

    fn loader() -> PathBuf { Self::contents().join("MacOS/loader") }

    fn payload_bundle() -> PathBuf { Self::contents().join("Resources/payload.bundle") }

    fn payload_plist() -> PathBuf { Self::payload_bundle().join("Contents/Info.plist") }

    fn payload() -> PathBuf { Self::payload_bundle().join("Contents/MacOS/payload") }
}

fn osax_plist() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
<key>CFBundleDevelopmentRegion</key>
<string>en</string>
<key>CFBundleExecutable</key>
<string>loader</string>
<key>CFBundleIdentifier</key>
<string>git.acsandmann.rift-osax</string>
<key>CFBundleInfoDictionaryVersion</key>
<string>6.0</string>
<key>CFBundleName</key>
<string>rift</string>
<key>CFBundlePackageType</key>
<string>osax</string>
<key>CFBundleShortVersionString</key>
<string>{OSAX_VERSION}</string>
<key>CFBundleVersion</key>
<string>{OSAX_VERSION}</string>
<key>OSAXHandlers</key>
<dict>
</dict>
</dict>
</plist>"#
    )
}

fn payload_plist() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
<key>CFBundleDevelopmentRegion</key>
<string>en</string>
<key>CFBundleExecutable</key>
<string>payload</string>
<key>CFBundleIdentifier</key>
<string>git.acsandmann.rift-sa</string>
<key>CFBundleInfoDictionaryVersion</key>
<string>6.0</string>
<key>CFBundleName</key>
<string>payload</string>
<key>CFBundlePackageType</key>
<string>BNDL</string>
<key>CFBundleShortVersionString</key>
<string>{OSAX_VERSION}</string>
<key>CFBundleVersion</key>
<string>{OSAX_VERSION}</string>
<key>NSPrincipalClass</key>
<string></string>
</dict>
</plist>"#
    )
}

fn is_installed() -> bool { Paths::base().is_dir() }

/// The `CFBundleVersion` of the payload bundle on disk, if one is installed.
///
/// The plist is one this module wrote, so a plain scan for the key beats
/// pulling in a plist parser.
fn installed_version() -> Option<String> {
    let plist = fs::read_to_string(Paths::payload_plist()).ok()?;
    let after_key = plist.split_once("<key>CFBundleVersion</key>")?.1;
    let value = after_key.split_once("<string>")?.1;
    let (version, _) = value.split_once("</string>")?;
    Some(version.trim().to_string())
}

/// Whether what is on disk is this build's payload.
fn is_current() -> bool { installed_version().is_some_and(|version| version == OSAX_VERSION) }

fn remove() -> io::Result<()> {
    match fs::remove_dir_all(Paths::base()) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

fn write_file(path: &Path, bytes: &[u8], mode: u32) -> Result<(), String> {
    fs::write(path, bytes)
        .map_err(|error| format!("failed to write {}: {error}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|error| format!("failed to chmod {}: {error}", path.display()))
}

/// Writes the bundle out fresh, replacing whatever was there.
///
/// Any failure past the first directory leaves a half-written bundle that Dock
/// could still try to load, so the whole thing is torn down on the way out.
fn install() -> Result<(), String> {
    if let Err(error) = remove() {
        return Err(format!("failed to replace {OSAX_BASE_DIR}: {error}"));
    }

    let result = install_inner();
    if result.is_err() {
        let _ = remove();
    }
    result
}

fn install_inner() -> Result<(), String> {
    for dir in [
        Paths::loader().parent().expect("loader has a parent").to_path_buf(),
        Paths::payload().parent().expect("payload has a parent").to_path_buf(),
    ] {
        fs::create_dir_all(&dir)
            .map_err(|error| format!("failed to create {}: {error}", dir.display()))?;
    }
    for dir in [Paths::base(), Paths::contents(), Paths::payload_bundle()] {
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o755));
    }

    write_file(&Paths::info_plist(), osax_plist().as_bytes(), 0o644)?;
    write_file(&Paths::payload_plist(), payload_plist().as_bytes(), 0o644)?;
    write_file(&Paths::loader(), LOADER_BIN, 0o755)?;
    write_file(&Paths::payload(), PAYLOAD_BIN, 0o755)?;

    // The loader must match the Dock it is about to spawn a thread in, and both
    // binaries must carry a signature for dyld to accept them.
    if let Err(error) = patch_loader_pac_abi() {
        eprintln!("rift: could not normalize the loader's arm64e PAC ABI: {error}");
    }
    codesign(&Paths::loader());
    codesign(&Paths::payload());

    Ok(())
}

fn codesign(path: &Path) {
    let status = Command::new("/usr/bin/codesign")
        .args(["-f", "-s", "-"])
        .arg(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if !matches!(status, Ok(status) if status.success()) {
        eprintln!("rift: failed to ad-hoc sign {}", path.display());
    }
}

fn restart_dock() { let _ = Command::new("/usr/bin/killall").arg("Dock").status(); }

//
// The arm64e capability byte of the loader has to equal the running Dock's, or
// the thread it spawns there is rejected. Both binaries carry that byte twice
// -- once in the fat header, once in the mach header -- and the two are
// independent on disk.
//

const MACHO_CPU_TYPE_ARM64: u32 = 16_777_228;
const MACHO_CPU_SUBTYPE_ARM64E: u32 = 2;
const MACHO_CPU_SUBTYPE_MASK: u32 = 0x00FF_FFFF;
const MACHO_FAT_MAGIC: u32 = 0xCAFE_BABE;
const MACHO_MH_MAGIC_64: u32 = 0xFEED_FACF;

/// Where a binary's arm64e capability byte lives, and what it currently says.
struct Arm64eCaps {
    caps: u8,
    /// Absent for a thin binary, which has no fat header.
    fat_offset: Option<u64>,
    mach_offset: Option<u64>,
}

fn read_u32(file: &File, offset: u64, big_endian: bool) -> io::Result<u32> {
    let mut bytes = [0u8; 4];
    file.read_exact_at(&mut bytes, offset)?;
    Ok(if big_endian {
        u32::from_be_bytes(bytes)
    } else {
        u32::from_le_bytes(bytes)
    })
}

/// Locates the arm64e slice of `file`.
///
/// Only 32-bit fat (`FAT_MAGIC`) is handled, not `FAT_MAGIC_64` with its
/// 32-byte `fat_arch_64` stride; Dock and the loader are both 32-bit fat
/// today. Returns `None` when there is no arm64e slice to speak of, which is
/// the ordinary answer on an Intel machine.
fn find_arm64e_caps(file: &File) -> io::Result<Option<Arm64eCaps>> {
    if read_u32(file, 0, true)? == MACHO_FAT_MAGIC {
        let arch_count = read_u32(file, 4, true)?;
        for index in 0..u64::from(arch_count) {
            let arch_offset = 8 + index * 20;
            let cpu_type = read_u32(file, arch_offset, true)?;
            let cpu_subtype = read_u32(file, arch_offset + 4, true)?;
            let slice_offset = u64::from(read_u32(file, arch_offset + 8, true)?);

            if cpu_type != MACHO_CPU_TYPE_ARM64 {
                continue;
            }
            if cpu_subtype & MACHO_CPU_SUBTYPE_MASK != MACHO_CPU_SUBTYPE_ARM64E {
                continue;
            }

            // The fat header claims arm64e; the slice itself must agree, or the
            // file is not one we understand well enough to patch.
            if read_u32(file, slice_offset, false)? != MACHO_MH_MAGIC_64 {
                return Ok(None);
            }
            if read_u32(file, slice_offset + 4, false)? != MACHO_CPU_TYPE_ARM64 {
                return Ok(None);
            }
            let slice_subtype = read_u32(file, slice_offset + 8, false)?;
            if slice_subtype & MACHO_CPU_SUBTYPE_MASK != MACHO_CPU_SUBTYPE_ARM64E {
                return Ok(None);
            }

            return Ok(Some(Arm64eCaps {
                caps: (cpu_subtype >> 24) as u8,
                // The fat header is big-endian, so the capability byte is the
                // first of the four; the mach header is little-endian, so it is
                // the last.
                fat_offset: Some(arch_offset + 4),
                mach_offset: Some(slice_offset + 11),
            }));
        }
        return Ok(None);
    }

    if read_u32(file, 0, false)? == MACHO_MH_MAGIC_64 {
        if read_u32(file, 4, false)? != MACHO_CPU_TYPE_ARM64 {
            return Ok(None);
        }
        let cpu_subtype = read_u32(file, 8, false)?;
        if cpu_subtype & MACHO_CPU_SUBTYPE_MASK != MACHO_CPU_SUBTYPE_ARM64E {
            return Ok(None);
        }
        return Ok(Some(Arm64eCaps {
            caps: (cpu_subtype >> 24) as u8,
            fat_offset: None,
            mach_offset: Some(11),
        }));
    }

    Ok(None)
}

/// Copies the running Dock's arm64e capability byte into the installed loader.
///
/// Returns whether anything was written, so the caller can re-sign only when it
/// has to. "Needs patching" is decided from *both* bytes rather than the fat
/// one alone: a previous partial write that left them out of sync would
/// otherwise read as already-correct and never be repaired. The mach byte is
/// written first and the fat byte last, since the fat byte is what the finder
/// keys on -- an interrupted patch leaves it stale and is re-detected next run.
fn patch_loader_pac_abi() -> io::Result<bool> {
    if !cfg!(target_arch = "aarch64") {
        return Ok(false);
    }

    let dock = File::open(DOCK_BINARY)?;
    let Some(dock_caps) = find_arm64e_caps(&dock)? else {
        return Ok(false);
    };

    let loader = File::options().read(true).write(true).open(Paths::loader())?;
    let Some(caps) = find_arm64e_caps(&loader)? else {
        return Ok(false);
    };

    let mut needs_patch = false;
    for offset in [caps.fat_offset, caps.mach_offset].into_iter().flatten() {
        let mut byte = [0u8; 1];
        loader.read_exact_at(&mut byte, offset)?;
        if byte[0] != dock_caps.caps {
            needs_patch = true;
        }
    }
    if !needs_patch {
        return Ok(false);
    }

    if let Some(offset) = caps.mach_offset {
        loader.write_all_at(&[dock_caps.caps], offset)?;
    }
    if let Some(offset) = caps.fat_offset {
        loader.write_all_at(&[dock_caps.caps], offset)?;
    }
    loader.sync_all()?;

    Ok(true)
}

//
// Preconditions. Each is reported rather than worked around: every one of them
// is a deliberate machine-wide setting, and silently doing half the job here
// would leave space commands failing for reasons nothing explains.
//

unsafe extern "C" {
    fn csr_get_active_config(config: *mut u32) -> i32;
    fn sysctlbyname(
        name: *const std::ffi::c_char,
        oldp: *mut std::ffi::c_void,
        oldlenp: *mut usize,
        newp: *const std::ffi::c_void,
        newlen: usize,
    ) -> i32;
}

const CSR_ALLOW_UNRESTRICTED_FS: u32 = 0x02;
const CSR_ALLOW_TASK_FOR_PID: u32 = 0x04;

fn is_sip_friendly() -> bool {
    let mut config = 0u32;
    // SAFETY: `csr_get_active_config` writes one u32 through the pointer.
    unsafe { csr_get_active_config(&mut config) };
    config & CSR_ALLOW_UNRESTRICTED_FS != 0 && config & CSR_ALLOW_TASK_FOR_PID != 0
}

/// Whether the kernel was booted with `-arm64e_preview_abi`, without which a
/// third-party arm64e binary will not run at all.
fn is_arm64e_enabled() -> bool {
    if !cfg!(target_arch = "aarch64") {
        return true;
    }

    let name = c"kern.bootargs";
    let mut buffer = [0u8; 2048];
    let mut len = buffer.len();
    // SAFETY: `name` is a NUL-terminated C string and `len` is the true size of
    // `buffer`; sysctl writes at most that many bytes and updates `len`.
    let result = unsafe {
        sysctlbyname(
            name.as_ptr(),
            buffer.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null(),
            0,
        )
    };
    if result != 0 {
        return false;
    }

    let bootargs = String::from_utf8_lossy(&buffer[..len.min(buffer.len())]);
    bootargs.contains("-arm64e_preview_abi")
}

fn require_root(verb: &str) -> Result<(), String> {
    if getuid().is_root() {
        Ok(())
    } else {
        Err(format!(
            "the scripting addition must be {verb} as root: run 'sudo rift sa ...'"
        ))
    }
}

fn require_sip_friendly() -> Result<(), String> {
    if is_sip_friendly() {
        Ok(())
    } else {
        Err(
            "System Integrity Protection: Filesystem Protections and Debugging Restrictions \
             must be disabled (recovery mode: 'csrutil enable --without fs --without debug \
             --without nvram')"
                .to_string(),
        )
    }
}

/// The socket to talk to once the payload is up.
///
/// `load` runs as root, and the payload names its socket after the user Dock
/// runs as -- the one who invoked sudo, not root.
fn invoking_user() -> Option<String> {
    if !getuid().is_root() {
        return std::env::var("USER").ok();
    }
    if let Ok(uid) = std::env::var("SUDO_UID")
        && let Ok(uid) = uid.parse::<u32>()
        && let Ok(Some(user)) = User::from_uid(Uid::from_raw(uid))
    {
        return Some(user.name);
    }
    std::env::var("SUDO_USER").ok()
}

/// Runs the installed loader, which injects the payload into Dock.
fn inject_once() -> Result<(), String> {
    let output = Command::new(Paths::loader())
        .output()
        .map_err(|error| format!("failed to run the loader: {error}"))?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr.trim();
    Err(if detail.is_empty() {
        "failed to inject the payload into Dock".to_string()
    } else {
        format!("failed to inject the payload into Dock: {detail}")
    })
}

/// Injects, waiting for Dock when it is still on its way back up.
///
/// The loader only acts on a Dock that reports itself finished launching, so
/// the attempt right after a restart is expected to fail for a second or two.
fn inject_payload(dock_restarting: bool) -> Result<(), String> {
    if !dock_restarting {
        return inject_once();
    }

    let mut last = None;
    for _ in 0..20 {
        match inject_once() {
            Ok(()) => return Ok(()),
            Err(error) => last = Some(error),
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(last.unwrap_or_else(|| "failed to inject the payload into Dock".to_string()))
}

/// Handshakes with a payload that was just injected.
///
/// The loader returns once the thread it spawned in Dock is running, which is
/// before that thread's `dlopen` has finished and the payload has bound its
/// socket -- so the first handshake is expected to find nothing.
fn handshake_after_inject(path: &str) -> Option<scripting_addition::Handshake> {
    for _ in 0..20 {
        if let Some(handshake) = scripting_addition::handshake(path) {
            return Some(handshake);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

/// Installs if needed, injects, and reports what the payload says for itself.
fn load() -> Result<String, String> {
    require_root("loaded")?;
    require_sip_friendly()?;

    if !is_arm64e_enabled() {
        return Err(
            "missing the required nvram boot-arg '-arm64e_preview_abi' (set it with \
             'sudo nvram boot-args=-arm64e_preview_abi' and reboot)"
                .to_string(),
        );
    }

    let user = invoking_user();
    let socket = user.as_deref().map(scripting_addition::socket_path_for_user);

    // A payload already inside Dock cannot be swapped in place -- dlopen of a
    // path already loaded is a no-op, and the old code keeps the socket. Only a
    // *different* version is worth restarting Dock over.
    let stale = socket
        .as_deref()
        .and_then(scripting_addition::handshake)
        .is_some_and(|live| live.version != OSAX_VERSION);

    if !is_installed() || !is_current() {
        install()?;
    } else {
        // The bundle is this build's, but the Dock it has to match may not be
        // the one it was written for -- a macOS update moves that byte.
        match patch_loader_pac_abi() {
            Ok(true) => codesign(&Paths::loader()),
            Ok(false) => {}
            Err(error) => eprintln!("rift: could not check the loader's arm64e PAC ABI: {error}"),
        }
    }

    if stale {
        restart_dock();
    }

    inject_payload(stale)?;

    let Some(path) = socket else {
        return Ok(format!(
            "scripting addition v{OSAX_VERSION} injected into Dock (could not determine the \
             invoking user, so its health was not checked)"
        ));
    };

    match handshake_after_inject(&path) {
        None => Err(format!("injected the payload but it never answered on {path}")),
        Some(handshake) if handshake.version != OSAX_VERSION => Err(format!(
            "Dock is holding payload v{} but this build ships v{OSAX_VERSION}; restart Dock \
             and load again",
            handshake.version
        )),
        Some(handshake) if !handshake.missing().is_empty() => Ok(format!(
            "scripting addition v{OSAX_VERSION} loaded, but it could not find {} in this Dock; \
             the matching commands will not work{}",
            handshake.missing().join(", "),
            reapply_to_running_rift()
        )),
        Some(handshake) => Ok(format!(
            "scripting addition v{} loaded and healthy{}",
            handshake.version,
            reapply_to_running_rift()
        )),
    }
}

/// A rift already running applied its addition-backed settings — the space
/// switch animation above all — when *it* started, to whatever payload was
/// in Dock then. A payload loaded afterwards (a Dock restart, a crash, this
/// very command) has none of that until told again. Reloading rift's config
/// re-sends it; done here so `sa load` leaves a working setup, not one that
/// needs rift restarted too. Best effort: rift may not be running.
fn reapply_to_running_rift() -> &'static str {
    let request = rift_protocol::RiftRequest::ExecuteCommand {
        command: rift_protocol::RiftCommand::Config(rift_protocol::ConfigCommand::ReloadConfig),
    };
    match crate::ipc::RiftMachClient::connect().and_then(|client| client.send_request(&request)) {
        Ok(_) => "; rift re-applied its settings to it",
        Err(_) => {
            "; rift is not running or could not be reached, so restart it to apply the \
                   space switch animation"
        }
    }
}

/// Reports the live state without root and without injecting anything.
///
/// This asks the payload rather than the filesystem, so it answers for the Dock
/// that is actually running: a Dock restart drops the payload while leaving the
/// bundle installed and looking fine.
fn status() -> Result<String, String> {
    // The rule is reported whatever the payload says: a healthy payload with
    // a stale rule is exactly the setup that breaks at the next Dock restart.
    let rule = sudoers_rule_state().describe();
    let append = |line: String| format!("{line}\n{rule}");
    payload_status().map(append).map_err(append)
}

fn payload_status() -> Result<String, String> {
    let Some(user) = invoking_user() else {
        return Err("cannot determine the current user (env USER is not set)".to_string());
    };

    let path = scripting_addition::socket_path_for_user(&user);
    let Some(handshake) = scripting_addition::handshake(&path) else {
        return Err(format!(
            "scripting addition is NOT loaded (nothing answered on {path}); run \
             'sudo rift sa load'"
        ));
    };

    if handshake.version != OSAX_VERSION {
        return Err(format!(
            "scripting addition is loaded but OUTDATED (payload v{}, this build expects \
             v{OSAX_VERSION}); run 'sudo rift sa load'",
            handshake.version
        ));
    }

    let missing = handshake.missing();
    if !missing.is_empty() {
        return Err(format!(
            "scripting addition v{} is loaded but could not find {} in this Dock",
            handshake.version,
            missing.join(", ")
        ));
    }

    Ok(format!(
        "scripting addition is loaded and healthy (payload v{}, attributes {:#04x})",
        handshake.version, handshake.attributes
    ))
}

//
// The launchd service has no tty to type a password at, so `sudo rift sa load`
// from `run_on_start` needs a passwordless rule. It is pinned to the sha256 of
// the exact binary that installed it -- it stops authorizing the moment rift is
// rebuilt or replaced -- and to the `sa load` subcommand alone.
//

unsafe extern "C" {
    fn CC_SHA256(data: *const u8, len: u32, md: *mut u8) -> *mut u8;
}

fn sha256_hex(path: &Path) -> io::Result<String> {
    let bytes = fs::read(path)?;
    let len = u32::try_from(bytes.len())
        .map_err(|_| io::Error::other("executable is too large to hash"))?;

    let mut digest = [0u8; 32];
    // SAFETY: `bytes` holds `len` readable bytes and `digest` is the 32 bytes
    // CC_SHA256 writes.
    unsafe { CC_SHA256(bytes.as_ptr(), len, digest.as_mut_ptr()) };

    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn install_sudoers() -> Result<String, String> {
    require_root("installed")?;

    // We are root here, so USER reads "root"; the rule's first field must name
    // the user who invoked sudo.
    let user = std::env::var("SUDO_USER")
        .ok()
        .filter(|user| !user.is_empty())
        .ok_or("cannot determine the invoking user; run via 'sudo rift sa install-sudoers'")?;

    let exe = std::env::current_exe()
        .map_err(|error| format!("unable to retrieve the path of this executable: {error}"))?;
    let exe = exe
        .to_str()
        .ok_or_else(|| format!("non-UTF8 executable path: {}", exe.display()))?;

    let sha =
        sha256_hex(Path::new(exe)).map_err(|error| format!("unable to hash '{exe}': {error}"))?;
    let rule = format!("{user} ALL=(root) NOPASSWD: sha256:{sha} {exe} sa load\n");

    let tmp = Path::new(SUDOERS_TMP_PATH);
    write_file(tmp, rule.as_bytes(), 0o440)?;

    // Validate before moving it into place: a malformed line here can never
    // lock the user out of sudo.
    let validated = Command::new("/usr/sbin/visudo")
        .arg("-cf")
        .arg(tmp)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if !matches!(validated, Ok(status) if status.success()) {
        let _ = fs::remove_file(tmp);
        return Err("the generated sudoers rule failed 'visudo -c'; not installing".to_string());
    }

    fs::rename(tmp, SUDOERS_PATH).map_err(|error| {
        let _ = fs::remove_file(tmp);
        format!("failed to move the sudoers rule into place at {SUDOERS_PATH}: {error}")
    })?;

    Ok(format!(
        "installed a passwordless 'sa load' sudoers rule at {SUDOERS_PATH} for user '{user}'"
    ))
}

fn uninstall_sudoers() -> Result<String, String> {
    require_root("uninstalled")?;

    match fs::remove_file(SUDOERS_PATH) {
        Ok(()) => Ok(format!("removed the sudoers rule at {SUDOERS_PATH}")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Ok(format!("no sudoers rule installed at {SUDOERS_PATH}"))
        }
        Err(error) => Err(format!("failed to remove {SUDOERS_PATH}: {error}")),
    }
}

/// What the passwordless rule says about *this* binary, as far as an
/// unprivileged process can tell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SudoersRule {
    /// No rule authorizes `sa load` for this user without a password.
    Missing,
    /// The rule is pinned to this binary's digest.
    Current,
    /// The rule is pinned to some other binary's digest, so `sudo rift sa load`
    /// asks for a password; `pinned` is the path it names.
    Stale { pinned: String },
    /// The check itself could not be made.
    Unknown(String),
}

impl SudoersRule {
    pub fn describe(&self) -> String {
        match self {
            Self::Missing => format!(
                "no passwordless 'sudo rift sa load' rule for this user; run \
                 'sudo rift sa install-sudoers' if run_on_start loads the addition"
            ),
            Self::Current => {
                "the passwordless 'sudo rift sa load' rule is pinned to this binary".to_string()
            }
            Self::Stale { pinned } => format!(
                "the passwordless rule at {SUDOERS_PATH} is pinned to a different build of \
                 {pinned}, so 'sudo rift sa load' will ask for a password (rift was rebuilt, \
                 upgraded or moved since); run 'sudo rift sa install-sudoers'"
            ),
            Self::Unknown(error) => {
                format!("could not check the passwordless 'sa load' rule: {error}")
            }
        }
    }
}

/// The rule file is root-only, so this asks `sudo -l` instead, which lists the
/// user's rules digest and all. That listing is itself passwordless exactly
/// when the user has *some* NOPASSWD rule (sudoers' `listpw=any` default) --
/// and the rule looked for is one -- so a listing that wants a password is a
/// user without it. The digest, not the path, is what decides staleness: the
/// path the rule names and the one this process runs under may differ (the
/// launchd plist runs the keg, `sudo rift` finds the symlink) while the bytes
/// are the same.
pub fn sudoers_rule_state() -> SudoersRule {
    if !Path::new(SUDOERS_PATH).exists() {
        return SudoersRule::Missing;
    }

    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            return SudoersRule::Unknown(format!("cannot locate this executable: {error}"));
        }
    };
    let sha = match sha256_hex(&exe) {
        Ok(sha) => sha,
        Err(error) => {
            return SudoersRule::Unknown(format!("unable to hash '{}': {error}", exe.display()));
        }
    };

    let output = match Command::new("/usr/bin/sudo").args(["-n", "-l"]).output() {
        Ok(output) => output,
        Err(error) => return SudoersRule::Unknown(format!("could not run sudo: {error}")),
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("password") {
            return SudoersRule::Missing;
        }
        return SudoersRule::Unknown(format!("'sudo -n -l' failed: {}", stderr.trim()));
    }

    let rules = pinned_load_rules(&String::from_utf8_lossy(&output.stdout));
    if rules.iter().any(|(digest, _)| *digest == sha) {
        SudoersRule::Current
    } else if let Some((_, pinned)) = rules.into_iter().next() {
        SudoersRule::Stale { pinned }
    } else {
        SudoersRule::Missing
    }
}

/// Every `sha256:<digest> <path> sa load` in a `sudo -l` listing, as
/// (digest, path). sudo wraps long entries across lines, so this reads tokens
/// rather than lines.
fn pinned_load_rules(listing: &str) -> Vec<(String, String)> {
    let tokens: Vec<&str> = listing.split_whitespace().collect();
    tokens
        .windows(4)
        .filter_map(|window| match window {
            [digest, path, "sa", "load"] if path.starts_with('/') => {
                let digest = digest.strip_prefix("sha256:")?;
                Some((digest.to_ascii_lowercase(), (*path).to_string()))
            }
            _ => None,
        })
        .collect()
}

/// `run_on_start` is where `sudo rift sa load` lives, and the launchd service
/// has no tty for the password sudo asks for once the rule no longer matches
/// this binary. Said up front, naming the fix, rather than left to the
/// command's own "a password is required" in the log.
pub fn warn_if_sudoers_rule_is_stale(run_on_start: &[String]) {
    let loads_via_sudo = run_on_start.iter().any(|command| {
        let tokens: Vec<&str> = command.split_whitespace().collect();
        tokens.first() == Some(&"sudo") && tokens.ends_with(&["sa", "load"])
    });
    if !loads_via_sudo {
        return;
    }
    match sudoers_rule_state() {
        SudoersRule::Current => {}
        state => tracing::warn!("{}", state.describe()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_rules_survive_sudo_wrapping_the_listing() {
        // What `sudo -l` prints on a machine with rift's rule and yabai's next
        // to it, wrapped the way sudo wraps.
        let listing = "User eric may run the following commands on host:\n    (ALL) ALL\n    \
                       (root) NOPASSWD:\n        \
                       sha256:943905C34467f2fc569f7730728df1c32123e47771f86acde8b7ab1b7c684f81\n        \
                       /opt/homebrew/bin/rift sa load\n    (root) NOPASSWD:\n        \
                       sha256:0cd2e1bd2b2ee394eb04c2cd48a42ce6be027eb79be9a4311ca7862b2fd0bcb6\n        \
                       /opt/homebrew/bin/yabai --load-sa\n";
        assert_eq!(pinned_load_rules(listing), vec![(
            "943905c34467f2fc569f7730728df1c32123e47771f86acde8b7ab1b7c684f81".to_string(),
            "/opt/homebrew/bin/rift".to_string()
        )]);
        assert!(pinned_load_rules("(ALL) ALL").is_empty());
    }

    #[test]
    fn version_matches_the_payload_header() {
        // The payload answers handshakes with its own compiled-in version, so
        // the two definitions have to be kept in step by hand.
        let header = include_str!("../osax/common.h");
        let line = header
            .lines()
            .find(|line| line.contains("#define OSAX_VERSION"))
            .expect("common.h defines OSAX_VERSION");
        assert!(
            line.contains(&format!("\"{OSAX_VERSION}\"")),
            "src/osax/common.h says {line:?}, but sys::osax says {OSAX_VERSION}"
        );
    }

    #[test]
    fn bundle_paths_agree_with_the_loader() {
        // The loader has the payload's absolute path compiled in, and nothing
        // at runtime tells it otherwise.
        let loader = include_str!("../osax/loader.m");
        let payload = Paths::payload();
        assert!(
            loader.contains(payload.to_str().unwrap()),
            "loader.m does not point at {}",
            payload.display()
        );
    }

    #[test]
    fn installed_version_reads_the_plist_this_module_writes() {
        let plist = payload_plist();
        let after_key = plist.split_once("<key>CFBundleVersion</key>").unwrap().1;
        let value = after_key.split_once("<string>").unwrap().1;
        assert_eq!(value.split_once("</string>").unwrap().0, OSAX_VERSION);
    }
}
