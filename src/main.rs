use gtk4::prelude::*;
use mterm::ui::build_ui;

fn main() -> gtk4::glib::ExitCode {
    if std::env::var_os("GSK_RENDERER").is_none() {
        // SAFETY: called before GTK or any other thread starts, so nothing can race on the environment.
        unsafe {
            std::env::set_var("GSK_RENDERER", "cairo");
        }
    }

    // GTK must run on the process main thread.
    let app = gtk4::Application::builder()
        .application_id("org.midnightbsd.mterm")
        .build();

    app.connect_activate(|app| {
        build_ui(app);
    });

    app.run()
}
