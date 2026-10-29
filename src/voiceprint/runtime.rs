//! ONNX Runtime provisioning. Linux loads Microsoft's official build at runtime; elsewhere it is linked in.

#[cfg(target_os = "linux")]
mod imp {
    use std::ffi::{CStr, c_char, c_void};
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, OnceLock};

    use sha2::{Digest, Sha256};

    pub const VERSION: &str = "1.22.0";
    const URL: &str = "https://github.com/microsoft/onnxruntime/releases/download/v1.22.0/onnxruntime-linux-x64-1.22.0.tgz";
    const SHA256: &str = "8344d55f93d5bc5021ce342db50f62079daf39aaafb5d311a451846228be49b3";
    const LIB: &str = "libonnxruntime.so.1.22.0";
    pub const GLIBCXX: &str = "GLIBCXX_3.4.22";

    pub fn dir() -> PathBuf {
        crate::config::snippet_home().join("lib").join(format!("onnxruntime-{VERSION}"))
    }

    pub fn lib_path() -> PathBuf {
        dir().join(LIB)
    }

    pub fn installed() -> Option<PathBuf> {
        let p = lib_path();
        p.is_file().then_some(p)
    }

    pub fn install() -> Result<PathBuf, String> {
        if let Some(p) = installed() {
            return Ok(p);
        }
        if !cfg!(target_arch = "x86_64") {
            return Err("no prebuilt voice runtime for this CPU".into());
        }
        let bytes = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .map_err(|e| e.to_string())?
            .get(URL)
            .send()
            .and_then(|r| r.error_for_status())
            .and_then(|r| r.bytes())
            .map_err(|e| format!("download {URL}: {e}"))?;
        let got = format!("{:x}", Sha256::digest(&bytes));
        if got != SHA256 {
            return Err(format!("onnxruntime {VERSION} checksum mismatch: {got}"));
        }
        let dest = dir();
        let tmp = dest.with_extension(format!("part-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
        extract(&bytes, &tmp).inspect_err(|_| {
            let _ = std::fs::remove_dir_all(&tmp);
        })?;
        let _ = std::fs::remove_dir_all(&dest);
        std::fs::rename(&tmp, &dest).map_err(|e| e.to_string())?;
        installed().ok_or_else(|| format!("{LIB} missing from the onnxruntime archive"))
    }

    fn extract(tgz: &[u8], to: &Path) -> Result<(), String> {
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(tgz));
        for entry in archive.entries().map_err(|e| e.to_string())? {
            let mut entry = entry.map_err(|e| e.to_string())?;
            let path = entry.path().map_err(|e| e.to_string())?.into_owned();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
            if name.starts_with("libonnxruntime.so") {
                entry.unpack(to.join(name)).map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }

    /// Real dlopen plus an API-version check, so ort can never panic on a bad library later.
    pub fn probe(path: &Path) -> Result<String, String> {
        #[repr(C)]
        struct ApiBase {
            get_api: unsafe extern "C" fn(u32) -> *const c_void,
            version: unsafe extern "C" fn() -> *const c_char,
        }
        let lib = unsafe { libloading::Library::new(path) }.map_err(|e| e.to_string())?;
        let base = unsafe { lib.get::<unsafe extern "C" fn() -> *const ApiBase>(b"OrtGetApiBase") }.map_err(|e| e.to_string())?;
        let base = unsafe { base() };
        if base.is_null() {
            return Err("OrtGetApiBase returned null".into());
        }
        let version = unsafe { CStr::from_ptr(((*base).version)()) }.to_string_lossy().into_owned();
        if unsafe { ((*base).get_api)(ort::sys::ORT_API_VERSION) }.is_null() {
            return Err(format!("onnxruntime {version} lacks API version {}", ort::sys::ORT_API_VERSION));
        }
        // ONNX Runtime does not survive dlclose; ort reuses this handle anyway.
        std::mem::forget(lib);
        Ok(version)
    }

    /// Installs and probes; a library that fails for reasons other than the system libstdc++ is fetched again once.
    pub fn install_checked() -> Result<(PathBuf, String), String> {
        let path = install()?;
        match probe(&path) {
            Ok(v) => Ok((path, v)),
            Err(e) if e.contains("GLIBC") || e.contains("libstdc++") => Err(format!("cannot load onnxruntime {VERSION} ({e})")),
            Err(_) => {
                let _ = std::fs::remove_dir_all(dir());
                let path = install()?;
                let v = probe(&path).map_err(|e| format!("cannot load onnxruntime {VERSION} ({e})"))?;
                Ok((path, v))
            }
        }
    }

    pub fn ensure() -> Result<(), String> {
        static READY: OnceLock<()> = OnceLock::new();
        static LOCK: Mutex<()> = Mutex::new(());
        if READY.get().is_some() {
            return Ok(());
        }
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if READY.get().is_some() {
            return Ok(());
        }
        let (path, _) = install_checked()?;
        ort::init_from(path.to_string_lossy()).commit().map_err(|e| e.to_string())?;
        let _ = READY.set(());
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    pub fn ensure() -> Result<(), String> {
        Ok(())
    }
}

pub use imp::*;
