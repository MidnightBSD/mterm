use gtk4::prelude::*;
use term::ui::build_ui;

fn main() -> gtk4::glib::ExitCode {
    // SAFETY: called before GTK or any other thread starts, so nothing can race on the environment.
    // Remember the user's IM module so shells (and GTK apps started from them) inherit the original.
    term::pty::set_child_gtk_im_module(std::env::var_os("GTK_IM_MODULE"));

    unsafe {
        if std::env::var_os("GSK_RENDERER").is_none() {
            std::env::set_var("GSK_RENDERER", "cairo");
        }

        // The IBus GTK4 input-method module (libim-ibus.so) recurses infinitely as soon as any
        // GtkEntry/GtkText exists, blowing the stack (SIGBUS/SIGSEGV) a few seconds after startup.
        // The terminal canvas encodes raw keyvals and never uses an IM context, so default to GTK's
        // built-in simple IM. Set TERM_RS_IM_MODULE to choose a different module explicitly.
        match std::env::var("TERM_RS_IM_MODULE") {
            Ok(module) if !module.is_empty() => std::env::set_var("GTK_IM_MODULE", module),
            _ => {
                let current = std::env::var("GTK_IM_MODULE").unwrap_or_default();
                if current.is_empty() || current.eq_ignore_ascii_case("ibus") {
                    std::env::set_var("GTK_IM_MODULE", "gtk-im-context-simple");
                }
            }
        }
    }

    // GTK must run on the process main thread.
    let app = gtk4::Application::builder()
        .application_id("org.midnightbsd.xfce_style_term")
        .build();

    app.connect_activate(|app| {
        build_ui(app);
    });

    app.run()
}
