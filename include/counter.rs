use anyhow::Result;
use thiserror::Error;
use tracing::debug;

#[derive(Debug, Default)]
pub struct Counter {
    value: u64,
    snapshot_after: Option<u64>,
}

impl Counter {
    pub fn with_snapshot_after(self, snapshot_after: u64) -> Self {
        Self {
            snapshot_after: Some(snapshot_after),
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    Inc(u64),
    Dec(u64),
}

//include!(concat!(env!("OUT_DIR"), "/counter.rs"));

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum Error {
    #[error("Overflow: value={value}, increment={inc}")]
    Overflow { value: u64, inc: u64 },
    #[error("Underflow: value={value}, decrement={dec}")]
    Underflow { value: u64, dec: u64 },
}

impl EventSourced for Counter {
    type Cmd = Cmd;

    type Evt = Evt;

    type State = u64;

    type Error = Error;

    fn handle_cmd(&self, cmd: Self::Cmd) -> Result<Vec<Self::Evt>, Self::Error> {
        match cmd {
            Cmd::Inc(inc) => {
                if inc > u64::MAX - self.value {
                    Err(Error::Overflow {
                        value: self.value,
                        inc,
                    })
                } else {
                    Ok(vec![Evt {
                        evt: Some(evt::Evt::Increased(Increased {
                            old_value: self.value,
                            inc,
                        })),
                    }])
                }
            }
            Cmd::Dec(dec) => {
                if dec > self.value {
                    Err(Error::Underflow {
                        value: self.value,
                        dec,
                    })
                } else {
                    Ok(vec![Evt {
                        evt: Some(evt::Evt::Decreased(Decreased {
                            old_value: self.value,
                            dec,
                        })),
                    }])
                }
            }
        }
    }

    fn handle_evt(&mut self, seq_no: u64, evt: &Self::Evt) -> Option<Self::State> {
        match evt.evt {
            Some(evt::Evt::Increased(Increased { old_value, inc })) => {
                self.value += inc;
                debug!(seq_no, old_value, inc, value = self.value, "Increased");
            }
            Some(evt::Evt::Decreased(Decreased { old_value, dec })) => {
                self.value -= dec;
                debug!(seq_no, old_value, dec, value = self.value, "Decreased");
            }
            None => panic!("evt is a mandatory field"),
        }

        self.snapshot_after.and_then(|snapshot_after| {
            if seq_no % snapshot_after == 0 {
                Some(self.value)
            } else {
                None
            }
        })
    }

    fn set_state(&mut self, state: Self::State) {
        self.value = state;
        debug!(value = self.value, "Set state");
    }
}
