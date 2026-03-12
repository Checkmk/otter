use super::StepExecutor;
use crate::types::{StepContext, StepDef, StepError, StepOutput};
use async_trait::async_trait;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal;
use std::io::{self, BufRead, Write};

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

        if ctx.feedback_available {
            print!("(c)ontinue, (s)top, or give (f)eedback: ");
        } else {
            print!("(c)ontinue or (s)top: ");
        }
        io::stdout().flush()?;

        match read_single_key(ctx.feedback_available)? {
            CheckpointAction::Continue => Ok(StepOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: Some(0),
                accepted: Some(true),
                feedback: None,
            }),
            CheckpointAction::Stop => Err(StepError::Rejected),
            CheckpointAction::Feedback => {
                print!("Feedback: ");
                io::stdout().flush()?;
                let mut line = String::new();
                io::stdin()
                    .lock()
                    .read_line(&mut line)
                    .map_err(|e| StepError::Io(e))?;
                let text = line.trim().to_string();
                Ok(StepOutput {
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: Some(0),
                    accepted: None,
                    feedback: Some(text),
                })
            }
        }
    }
}

enum CheckpointAction {
    Continue,
    Stop,
    Feedback,
}

fn read_single_key(feedback_available: bool) -> Result<CheckpointAction, StepError> {
    terminal::enable_raw_mode().map_err(|e| StepError::Io(io::Error::other(e)))?;

    // Drain any buffered key events (e.g. leftover from prior stdin read_line)
    while event::poll(std::time::Duration::ZERO).unwrap_or(false) {
        let _ = event::read();
    }

    let result = loop {
        if let Ok(Event::Key(KeyEvent {
            code, modifiers, ..
        })) = event::read()
        {
            if modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
                break Err(StepError::Io(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "interrupted",
                )));
            }
            match code {
                KeyCode::Char('c') => break Ok(CheckpointAction::Continue),
                KeyCode::Char('s') => break Ok(CheckpointAction::Stop),
                KeyCode::Char('f') if feedback_available => break Ok(CheckpointAction::Feedback),
                _ => {}
            }
        }
    };
    terminal::disable_raw_mode().map_err(|e| StepError::Io(io::Error::other(e)))?;
    println!();
    result
}
