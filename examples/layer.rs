//! A layer-shell overlay with a text field: `cargo run --example layer
//! --features png`. Escape or a click
//! outside hides it for a second, then it comes back; Ctrl+Q quits.

use qui::wl::{self, Cx, Spec, Waker};
use qui::{Align, Color, Key, Rect, Ui, id};

struct Demo {
    text: String,
    waker: Waker,
    shown: u32,
}

impl Demo {
    fn spec() -> Spec {
        if std::env::args().any(|a| a == "--window") {
            return Spec { min_size: (300, 200), ..Spec::window("qui demo", "qui-demo", 520, 300) };
        }
        Spec { namespace: "qui-demo".into(), width: 520, height: 300, ..Spec::default() }
    }
    fn hide_for_a_second(&self, cx: &mut Cx) {
        cx.hide = true;
        let waker = self.waker.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(1));
            waker.wake();
        });
    }
}

impl wl::App for Demo {
    fn start(&mut self, cx: &mut Cx) {
        cx.show = Some(Self::spec());
    }

    fn build(&mut self, ui: &mut Ui, cx: &mut Cx) {
        let keys: Vec<(usize, qui::KeyEvent)> = ui.keys().map(|(i, k)| (i, k.clone())).collect();
        for (i, k) in keys {
            if k.key == Key::Named(qui::Named::Escape) {
                ui.consume_key(i);
                self.hide_for_a_second(cx);
            }
            if k.mods.ctrl && k.key == Key::Char('q') {
                cx.exit = true;
            }
        }
        let t = ui.theme;
        let r = Rect::new(0.0, 0.0, ui.width, ui.height);
        let card = r.inset(16.0, 16.0);
        ui.shadow(card, Color(0x000000, 120), 18.0, 14.0, 0.0, 6.0);
        ui.rect(card, t.bg, 18.0);
        ui.outline(card, t.border, 18.0, 1.0);
        let inner = card.inset(20.0, 20.0);
        ui.focus = Some(id("field", 0));
        ui.text_edit(id("field", 0), Rect::new(inner.x, inner.y, inner.w, 40.0), &mut self.text, "Type here", false);
        ui.text_in(Rect::new(inner.x, inner.y + 60.0, inner.w, 30.0), &format!("Shown {} times · emoji: 😀🎉 ✨ · Ünïcödé · 日本語", self.shown), 15.0, t.text, Align::Left);
        ui.text_in(Rect::new(inner.x, inner.y + 100.0, inner.w, 24.0), "Escape hides for a second; Ctrl+Q quits", 12.0, t.muted, Align::Left);
    }

    fn woken(&mut self, cx: &mut Cx) -> bool {
        if !cx.visible() {
            self.shown += 1;
            cx.show = Some(Self::spec());
        }
        true
    }

    fn clicked_outside(&mut self, cx: &mut Cx) {
        self.hide_for_a_second(cx);
    }
}

fn main() {
    if let Err(e) = wl::run(|waker| Demo { text: String::new(), waker, shown: 1 }) {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
