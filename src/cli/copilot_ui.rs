use crate::hooks::copilot::parse_copilot_input;
use crate::types::CopilotAction;
use std::io::{self, Write};

/// Display stage output and prompt user for copilot action.
pub fn prompt_copilot_action(stage: &str, output_preview: &str) -> io::Result<CopilotAction> {
    println!("\n--- {} output ---", stage);
    let preview = if output_preview.len() > 500 {
        format!(
            "{}...\n[{} more chars]",
            &output_preview[..500],
            output_preview.len() - 500
        )
    } else {
        output_preview.to_string()
    };
    println!("{}", preview);
    println!();
    println!("[a]ccept  [r]eject+feedback  [e]dit in $EDITOR");
    print!("> ");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(parse_copilot_input(input.trim()))
}
