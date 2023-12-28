use std::{io, sync::{atomic::{Ordering, AtomicBool}, Arc}, error::Error};

use crossterm::{terminal::{enable_raw_mode, EnterAlternateScreen, disable_raw_mode, LeaveAlternateScreen}, execute, event::{EnableMouseCapture, DisableMouseCapture}};
use parking_lot::RwLock;
use ratatui::{backend::CrosstermBackend, Terminal, layout::{Layout, Direction, Constraint}, widgets::{Paragraph, Block, Wrap, Borders}, text::{Span, Line, Text}, style::{Color, Style}};
use tracing::error;

use crate::{SELECTION_COLOUR, static_selection::StaticSelection, Focus};

pub async fn renderer_wrappers_wrapper(static_menu_selection: Arc<StaticSelection>, focus: Arc<RwLock<Focus>>, stop: Arc<AtomicBool>) -> Result<(), ()> {
    match renderer_wrapper(static_menu_selection.to_owned(), focus.to_owned(), stop.to_owned()).await {
        Ok(_) => return Ok(()),
        Err(err) => {
            error!("{}", err);
            return Err(())
        }
    }
    
}

pub async fn renderer_wrapper(static_menu_selection: Arc<StaticSelection>, focus: Arc<RwLock<Focus>>, stop: Arc<AtomicBool>) -> Result<(), Box<dyn Error>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal: Terminal<CrosstermBackend<io::Stdout>> = Terminal::new(backend)?;
    terminal.clear()?;

    let renderer_result = renderer(&mut terminal, static_menu_selection.to_owned(), focus.to_owned(), stop.to_owned()).await;

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    renderer_result
}

pub async fn renderer(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, static_menu_selection: Arc<StaticSelection>, focus: Arc<RwLock<Focus>>, stop: Arc<AtomicBool>) -> Result<(), Box<dyn Error>> {
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        terminal.draw(|f| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Max(100)].as_ref())
                .split(f.size());
            let top_paragraph = Paragraph::new(
                vec![
                    "H for the Help menu",
                    "Arrow keys for navigation",
                    "Enter to select/toggle",
                    "Page up/down for first/last element",
                    "F5 Start/Stop",
                ]
                .iter()
                .map(|&tip| format!("[{}] ", tip))
                .collect::<String>(),
            )
            .block(Block::default())
            .wrap(Wrap { trim: true });
            f.render_widget(top_paragraph, chunks[0]);
            let bottom_chunks = Layout::default()
                .direction(Direction::Horizontal)
                .margin(1)
                .constraints(
                    [
                        Constraint::Percentage(20), // Selection
                        Constraint::Percentage(20), // Controls
                        Constraint::Percentage(60), // Results
                    ]
                    .as_ref(),
                )
                .split(chunks[1]);

            let pane_selected = static_menu_selection.pane_selected.load(Ordering::SeqCst);

            let left_paragraph = Paragraph::new(static_menu_selection.generate_root_list()).block(
                Block::default()
                    .title(Span::styled(
                        "Root Selection",
                        Style::default().fg(Color::White),
                    ))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(if pane_selected == 0 {
                        SELECTION_COLOUR
                    } else {
                        Color::White
                    })),
            );
            f.render_widget(left_paragraph, bottom_chunks[0]);

            let mut controls: Vec<Line> = Vec::new();
            let running = static_menu_selection.running.load(Ordering::SeqCst);
            let run_control_disabled = static_menu_selection
                .run_control_temporarily_disabled
                .load(Ordering::SeqCst);
            controls.push(Line::from(Span::styled(
                if running {
                    if running && run_control_disabled {
                        "Stopping"
                    } else {
                        "Stop"
                    }
                } else {
                    "Start"
                },
                Style::default().fg(if running && !run_control_disabled {
                    Color::Green
                } else if running && run_control_disabled {
                    Color::Red
                } else {
                    Color::White
                }),
            )));

            let middle_column = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(92), Constraint::Percentage(8)].as_ref())
                .split(bottom_chunks[1]);

            let search_terms_paragraph = Paragraph::new(
                static_menu_selection
                    .search_term_tracker
                    .read()
                    .render(pane_selected == 1),
            )
            .block(
                Block::default()
                    .title(Span::styled(
                        "Search Terms",
                        Style::default().fg(Color::White),
                    ))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(if pane_selected == 1 {
                        SELECTION_COLOUR
                    } else {
                        Color::White
                    })),
            )
            .wrap(Wrap { trim: true });
            f.render_widget(search_terms_paragraph, middle_column[0]);
            let controls_paragraph = Paragraph::new(controls)
                .block(
                    Block::default()
                        .title(Span::styled("Controls", Style::default().fg(Color::White)))
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::White)),
                )
                .wrap(Wrap { trim: true });
            f.render_widget(controls_paragraph, middle_column[1]);

            let right_text = Text::from(static_menu_selection.generate_results());
            let right_paragraph = Paragraph::new(right_text).block(
                Block::default()
                    .title(Span::styled("Results", Style::default().fg(Color::White)))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(if pane_selected == 2 {
                        SELECTION_COLOUR
                    } else {
                        Color::White
                    })),
            );
            f.render_widget(right_paragraph, bottom_chunks[2]);

            //Renders overlay
            let focus = focus.read().to_owned();
            match focus {
                Focus::Main => {}
                _ => {
                    let vertical_split = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints(
                            [
                                Constraint::Ratio(1, 3),
                                Constraint::Ratio(1, 3),
                                Constraint::Ratio(1, 3),
                            ]
                            .as_ref(),
                        )
                        .split(f.size());
                    let horizontal_split = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints(
                            [
                                Constraint::Ratio(1, 3),
                                Constraint::Ratio(1, 3),
                                Constraint::Ratio(1, 3),
                            ]
                            .as_ref(),
                        )
                        .split(vertical_split[1]);
                    let middle_pane = horizontal_split[1];
                    let paragraph = match focus {
                        Focus::ConfirmClose => Paragraph::new("Y/N").block(
                            Block::default()
                                .title(Span::styled(
                                    "Confirm Close",
                                    Style::default().fg(Color::White),
                                ))
                                .style(Style::default().bg(Color::DarkGray))
                                .borders(Borders::ALL)
                                .border_style(Style::default().fg(Color::White)),
                        ),
                        Focus::Help => Paragraph::new("Placeholder").block(
                            Block::default()
                                .title(Span::styled(
                                    "Help/Controls",
                                    Style::default().fg(Color::White),
                                ))
                                .style(Style::default().bg(Color::DarkGray))
                                .borders(Borders::ALL)
                                .border_style(Style::default().fg(Color::White)),
                        ),
                        Focus::SearchMod(search_editor) => {
                            Paragraph::new(search_editor.read().as_ref().unwrap().render()).block(
                                Block::default()
                                    .title(Span::styled(
                                        "Search Modify",
                                        Style::default().fg(Color::White),
                                    ))
                                    .style(Style::default().bg(Color::DarkGray))
                                    .borders(Borders::ALL)
                                    .border_style(Style::default().fg(Color::White)),
                            )
                        }
                        Focus::Main => panic!(), //this case will never run
                    };
                    f.render_widget(paragraph, middle_pane);
                }
            }
        })?;
    }
    Ok(())
}