//! Everything the commands need from the outside world, behind one trait so tests can
//! capture output, script the confirmation prompt and substitute the RPC.

use std::io::{BufRead, Write};

use crate::error::{Error, Result};
use crate::rpc::{HttpRpc, Rpc};

pub trait Host {
    fn out(&mut self, s: &str);
    fn err(&mut self, s: &str);
    /// Ask the human a question and return the line they typed. The real implementation
    /// reads the terminal itself (not stdin), so a script cannot pipe an answer in.
    fn prompt(&mut self, question: &str) -> Result<String>;
    fn rpc(&mut self, url: &str) -> Result<Box<dyn Rpc>>;
}

pub struct RealHost;

impl Host for RealHost {
    fn out(&mut self, s: &str) {
        print!("{s}");
        let _ = std::io::stdout().flush();
    }

    fn err(&mut self, s: &str) {
        eprint!("{s}");
    }

    fn prompt(&mut self, question: &str) -> Result<String> {
        let tty = std::fs::OpenOptions::new().read(true).write(true).open("/dev/tty").map_err(|_| {
            Error("refused: there is no terminal to ask on; signing and sending need a human at a keyboard".into())
        })?;
        let mut w = tty.try_clone().map_err(|_| Error("refused: cannot use the terminal".into()))?;
        write!(w, "{question}").map_err(|_| Error("refused: cannot write to the terminal".into()))?;
        let _ = w.flush();
        let mut line = String::new();
        std::io::BufReader::new(tty)
            .read_line(&mut line)
            .map_err(|_| Error("refused: cannot read from the terminal".into()))?;
        Ok(line.trim_end_matches(['\n', '\r']).to_string())
    }

    fn rpc(&mut self, url: &str) -> Result<Box<dyn Rpc>> {
        Ok(Box::new(HttpRpc::new(url)))
    }
}
