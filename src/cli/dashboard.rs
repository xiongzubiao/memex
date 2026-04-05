use crate::types::*;

/// Render the convergence dashboard to a string for terminal display.
#[allow(clippy::too_many_arguments)]
pub fn render_dashboard(
    round: u32,
    max_rounds: u32,
    task_type: &str,
    sections: &[SectionConvergence],
    cost_usd: f64,
    tokens_in: u64,
    tokens_out: u64,
    warnings: &[String],
) -> String {
    let name_width = sections.iter().map(|s| s.name.len()).max().unwrap_or(10).max(10);
    // Fixed widths for status/trend/agreement columns
    let status_w = 9;
    let trend_w = 5;
    let agree_w = 15;
    // Total width: "| " + name + " | " + status + " | " + trend + " | " + agreement + " |"
    let width = 4 + name_width + 3 + status_w + 3 + trend_w + 3 + agree_w + 2;
    let mut out = String::new();

    out.push_str(&format!("+{}+\n", "-".repeat(width - 2)));
    out.push_str(&format!(
        "|  Round {}/{} | Standard pipeline | {:<w$}|\n",
        round,
        max_rounds,
        task_type,
        w = width - 38
    ));
    out.push_str(&format!("|{}|\n", "-".repeat(width - 2)));

    out.push_str(&format!(
        "|  {:<nw$} | {:<sw$} | {:<tw$} | {:<aw$} |\n",
        "Section", "Status", "Trend", "Agreement",
        nw = name_width, sw = status_w, tw = trend_w, aw = agree_w
    ));
    out.push_str(&format!(
        "|  {}+{}+{}+{} |\n",
        "-".repeat(name_width),
        "-".repeat(status_w + 2),
        "-".repeat(trend_w + 2),
        "-".repeat(agree_w + 3)
    ));

    for section in sections {
        let status = if section.converged {
            "CONVERGED"
        } else {
            &format!("Round {}", round)
        };
        let trend = match section.trend {
            Trend::Better => "^",
            Trend::Worse => "v",
            Trend::Same => "=",
        };
        let warn = if section.irreconcilable { " !" } else { "" };
        out.push_str(&format!(
            "|  {:<nw$} | {:<sw$} | {:<tw$} | {:<aw$}{} |\n",
            section.name, status, trend, section.agreement, warn,
            nw = name_width, sw = status_w, tw = trend_w, aw = agree_w
        ));
    }

    out.push_str(&format!("|{}|\n", "-".repeat(width - 2)));
    out.push_str(&format!(
        "|  Cost: ${:.2} | Tokens: {:.1}K in / {:.1}K out{:>pad$}|\n",
        cost_usd,
        tokens_in as f64 / 1000.0,
        tokens_out as f64 / 1000.0,
        "",
        pad = width.saturating_sub(50)
    ));

    for warning in warnings {
        let warn_pad = width.saturating_sub(6);
        out.push_str(&format!("|  ! {:<w$}|\n", warning, w = warn_pad));
    }

    out.push_str(&format!("+{}+\n", "-".repeat(width - 2)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dashboard_renders_sections() {
        let sections = vec![
            SectionConvergence {
                name: "Problem".into(),
                converged: true,
                trend: Trend::Same,
                agreement: "3/3 agree".into(),
                irreconcilable: false,
            },
            SectionConvergence {
                name: "Architecture".into(),
                converged: false,
                trend: Trend::Better,
                agreement: "2/3 agree".into(),
                irreconcilable: false,
            },
        ];
        let output = render_dashboard(2, 5, "Software Design", &sections, 1.23, 45200, 12800, &[]);
        assert!(output.contains("Round 2/5"));
        assert!(output.contains("Problem"));
        assert!(output.contains("CONVERGED"));
        assert!(output.contains("Architecture"));
        assert!(output.contains("$1.23"));
    }
}
