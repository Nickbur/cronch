fn main() {
    slint_build::compile("ui/app.slint").expect("failed to compile Slint UI");

    // Embed the application icon into the Windows executable (Explorer/taskbar).
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.compile().expect("failed to embed Windows resources");
    }
}
