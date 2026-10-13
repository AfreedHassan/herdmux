use opensessions_sidebar_core::{
    app::App, generated::protocol::ServerMessage, renderer::render_app,
};
use ratatui::{Terminal, backend::TestBackend};
use std::io::{self, Read};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let mut messages = serde_json::Deserializer::from_str(&input).into_iter::<ServerMessage>();
    let ServerMessage::State(state) =
        messages.next().ok_or("expected a server state message")??
    else {
        return Err("expected a server state message".into());
    };
    let mut app = App::from_state(state);
    for message in messages {
        app.apply_server_message(message?);
    }
    eprintln!(
        "highlightedAgentIndex={}",
        app.highlighted_agent_idx().map_or(-1, |index| index as i64)
    );
    let mut terminal = Terminal::new(TestBackend::new(30, 48))?;
    terminal.draw(|frame| render_app(frame, &app))?;
    let buffer = terminal.backend().buffer();
    for y in 0..48 {
        let line: String = (0..30).map(|x| buffer[(x, y)].symbol()).collect();
        println!("{}", line.trim_end());
    }
    Ok(())
}
