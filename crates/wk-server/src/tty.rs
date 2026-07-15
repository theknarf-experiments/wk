//! Host side of `wk:tty/control` — terminal line-discipline control, wk's
//! stand-in for the raw-mode terminal interface WASI hasn't standardized yet
//! (see `wit-tty/world.wit`). A terminal guest's portable `termios` shim maps
//! `tcgetattr`/`tcsetattr` onto `get`/`set` here; the mode lives on the node's
//! shared `TermIo`, which the client reads to choose raw vs. cooked input. This
//! replaces the old in-band `ESC[?7777h` escape, so neither wk nor the guest
//! carries knowledge of the other — both just speak this capability.

use wasmtime::component::{HasData, Linker};
use wasmtime::Result;

use crate::plugin::HostState;

wasmtime::component::bindgen!({
    path: "wit-tty",
    world: "tty-host",
    imports: { default: trappable },
    require_store_data_send: true,
});

pub fn add_to_linker(l: &mut Linker<HostState>) -> Result<()> {
    wk::tty::control::add_to_linker::<_, HasTty>(l, |s| s)?;
    Ok(())
}

struct HasTty;
impl HasData for HasTty {
    type Data<'a> = &'a mut HostState;
}

impl wk::tty::control::Host for HostState {
    fn get(&mut self) -> Result<wk::tty::control::State> {
        let mode = self.term_io.tty();
        let (cols, rows) = self.term_io.size();
        Ok(wk::tty::control::State {
            cols: cols as u32,
            rows: rows as u32,
            echo: mode.echo,
            canonical: mode.canonical,
        })
    }

    fn set(&mut self, echo: bool, canonical: bool) -> Result<()> {
        self.term_io.set_tty(echo, canonical);
        Ok(())
    }
}
