use super::StepExecutor;
use crate::types::{StepContext, StepDef, StepError, StepOutput};
use async_trait::async_trait;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal;
use std::io::{self, Write};

pub struct CheckpointExecutor;

#[async_trait]
impl StepExecutor for CheckpointExecutor {
    fn step_type(&self) -> crate::types::StepType {
        crate::types::StepType::Checkpoint
    }

    async fn execute(
        &self,
        step_def: &StepDef,
        ctx: &StepContext,
    ) -> Result<StepOutput, StepError> {
        let message = step_def.message.as_deref().unwrap_or("Checkpoint reached");

        println!("\n[CHECKPOINT] {}", message);
        println!("Scratch dir: {}", ctx.scratch_dir.display());
        print!("(c)ontinue or (s)top: ");
        io::stdout().flush()?;

        let accepted = read_single_key()?;

        if !accepted {
            return Err(StepError::Rejected);
        }

        Ok(StepOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(0),
            accepted: Some(true),
        })
    }
}

fn read_single_key() -> Result<bool, StepError> {
    terminal::enable_raw_mode().map_err(|e| StepError::Io(io::Error::other(e)))?;
    let result = loop {
        if let Ok(Event::Key(KeyEvent { code, modifiers, .. })) = event::read() {
            if modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
                break Err(StepError::Io(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "interrupted",
                )));
            }
            match code {
                KeyCode::Char('c') => break Ok(true),
                KeyCode::Char('s') => break Ok(false),
                _ => {}
            }
        }
    };
    terminal::disable_raw_mode().map_err(|e| StepError::Io(io::Error::other(e)))?;
    println!();
    result
}
