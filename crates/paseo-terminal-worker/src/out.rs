//! Single ordered stdout writer: every response/event goes through one channel so lines never
//! interleave and keep emission order.

use serde_json::Value;
use std::io::Write;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread::JoinHandle;

pub enum OutMsg {
    Line(String),
    Quit,
}

#[derive(Clone)]
pub struct Out(Sender<OutMsg>);

impl Out {
    pub fn send(&self, value: &Value) {
        let _ = self.0.send(OutMsg::Line(value.to_string()));
    }

    pub fn quit(&self) {
        let _ = self.0.send(OutMsg::Quit);
    }
}

fn run_writer<W: Write>(rx: Receiver<OutMsg>, mut w: W) {
    let write_line = |line: &str, w: &mut W| {
        let _ = w.write_all(line.as_bytes());
        let _ = w.write_all(b"\n");
    };
    while let Ok(msg) = rx.recv() {
        match msg {
            OutMsg::Quit => break,
            OutMsg::Line(line) => {
                write_line(&line, &mut w);
                let mut quit = false;
                while let Ok(next) = rx.try_recv() {
                    match next {
                        OutMsg::Line(l) => write_line(&l, &mut w),
                        OutMsg::Quit => {
                            quit = true;
                            break;
                        }
                    }
                }
                let _ = w.flush();
                if quit {
                    break;
                }
            }
        }
    }
    let _ = w.flush();
}

pub fn spawn_stdout_writer() -> (Out, JoinHandle<()>) {
    let (tx, rx) = channel();
    let handle = std::thread::Builder::new()
        .name("stdout-writer".into())
        .spawn(move || run_writer(rx, std::io::BufWriter::new(std::io::stdout())))
        .expect("spawn stdout writer");
    (Out(tx), handle)
}
