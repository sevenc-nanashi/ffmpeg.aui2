use anyhow::Context;

static DEPENDENCY_HANDLES: std::sync::Mutex<Vec<libloading::Library>> =
    std::sync::Mutex::new(Vec::new());
static CORE_HANDLE: std::sync::Mutex<Option<libloading::Library>> = std::sync::Mutex::new(None);

fn get_dependency_dir() -> std::path::PathBuf {
    process_path::get_dylib_path()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn try_initialize_core_handle() -> anyhow::Result<()> {
    let mut dependency_handles_lock = DEPENDENCY_HANDLES.lock().unwrap();
    if dependency_handles_lock.is_empty() {
        let dependency_paths = [
            "avutil-60.dll",
            "swresample-6.dll",
            "avcodec-62.dll",
            "avformat-62.dll",
            "swscale-9.dll",
            "avfilter-11.dll",
            "avdevice-62.dll",
        ];
        for dep in &dependency_paths {
            let dep_path = get_dependency_dir().join("dependencies").join(dep);
            dependency_handles_lock.push(unsafe {
                libloading::Library::new(dep_path)
                    .with_context(|| format!("Failed to load dependency: {}", dep))?
            });
        }
    }
    let mut core_handle_lock = CORE_HANDLE.lock().unwrap();
    if core_handle_lock.is_none() {
        let core_path = get_dependency_dir().join("ffmpeg_aui2_core.dll");
        *core_handle_lock = Some(unsafe {
            libloading::Library::new(core_path)
                .context("Failed to load core library: ffmpeg_aui2_core.dll")?
        });
    }
    Ok(())
}

fn initialize_core_handle() {
    let res = try_initialize_core_handle();
    match res {
        Ok(_) => (),
        Err(e) => native_dialog::DialogBuilder::message()
            .set_title("ffmpeg.aui2")
            .set_text(format!(
                "Failed to initialize core library: {e}\n\nPlease try reinstalling the plugin."
            ))
            .set_level(native_dialog::MessageLevel::Error)
            .alert()
            .show()
            .unwrap(),
    }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn RequiredVersion() -> u32 {
    initialize_core_handle();

    unsafe {
        CORE_HANDLE
            .lock()
            .unwrap()
            .as_ref()
            .expect("Core library not loaded")
            .get::<unsafe extern "C" fn() -> u32>(b"RequiredVersion\0")
            .expect("Failed to get RequiredVersion function from core library")()
    }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn InitializeLogger(logger: *mut aviutl2::sys::logger2::LOG_HANDLE) {
    unsafe {
        CORE_HANDLE
            .lock()
            .unwrap()
            .as_ref()
            .expect("Core library not loaded")
            .get::<unsafe extern "C" fn(*mut aviutl2::sys::logger2::LOG_HANDLE)>(
                b"InitializeLogger\0",
            )
            .expect("Failed to get InitializeLogger function from core library")(logger)
    }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn InitializeConfig(config: *mut aviutl2::sys::config2::CONFIG_HANDLE) {
    unsafe {
        CORE_HANDLE
            .lock()
            .unwrap()
            .as_ref()
            .expect("Core library not loaded")
            .get::<unsafe extern "C" fn(*mut aviutl2::sys::config2::CONFIG_HANDLE)>(
                b"InitializeConfig\0",
            )
            .expect("Failed to get InitializeConfig function from core library")(config)
    }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn InitializePlugin(version: u32) -> bool {
    unsafe {
        CORE_HANDLE
            .lock()
            .unwrap()
            .as_ref()
            .expect("Core library not loaded")
            .get::<unsafe extern "C" fn(u32) -> bool>(b"InitializePlugin\0")
            .expect("Failed to get InitializePlugin function from core library")(version)
    }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn UninitializePlugin() {
    unsafe {
        CORE_HANDLE
            .lock()
            .unwrap()
            .as_ref()
            .expect("Core library not loaded")
            .get::<unsafe extern "C" fn()>(b"UninitializePlugin\0")
            .expect("Failed to get UninitializePlugin function from core library")()
    }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn GetInputPluginTable() -> *mut aviutl2::sys::input2::INPUT_PLUGIN_TABLE {
    unsafe {
        CORE_HANDLE
            .lock()
            .unwrap()
            .as_ref()
            .expect("Core library not loaded")
            .get::<unsafe extern "C" fn() -> *mut aviutl2::sys::input2::INPUT_PLUGIN_TABLE>(
                b"GetInputPluginTable\0",
            )
            .expect("Failed to get GetInputPluginTable function from core library")()
    }
}
