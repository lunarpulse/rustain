use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::oneshot;

use super::conversation::{ChatMessage, ContentBlock, Conversation};
use super::event::ApprovalDecision;
use super::stream::TuiStreamEvent;

// ── Focus & Mode ────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Input,
    Chat,
    Sidebar,
}

#[derive(Debug, Clone)]
pub enum AppMode {
    Normal,
    PermissionPrompt {
        tool_name: String,
        tool_input: String,
        pending_tool_id: String,
    },
    PlanApproval { plan_content: String },
    AskUserQuestion { question: String, response_tool_id: String },
    SlashCommandDropdown { filter: String, selected: usize },
    MentionDropdown { filter: String, selected: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionMode {
    Yolo,
    Normal,
    Plan,
}

// ── Tab State ───────────────────────────────────────────────────

#[derive(Debug)]
pub struct TabState {
    pub id: String,
    pub conversation: Option<Conversation>,
    pub is_streaming: bool,
    pub scroll_offset: u16,
    pub auto_scroll: bool,
    pub service_initialized: bool,
}

impl TabState {
    pub fn new() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            conversation: None,
            is_streaming: false,
            scroll_offset: 0,
            auto_scroll: true,
            service_initialized: false,
        }
    }

    pub fn title(&self) -> &str {
        self.conversation
            .as_ref()
            .map(|c| c.title.as_str())
            .unwrap_or("New Chat")
    }

    /// Ensure conversation exists (lazy creation on first message)
    pub fn ensure_conversation(&mut self) -> &mut Conversation {
        if self.conversation.is_none() {
            self.conversation = Some(Conversation::new());
        }
        self.conversation.as_mut().unwrap()
    }
}

// ── Application State ───────────────────────────────────────────

pub struct AppState {
    pub tabs: Vec<TabState>,
    pub active_tab: usize,
    pub focus: Focus,
    pub mode: AppMode,
    pub input_buffer: String,
    pub cursor_position: usize,
    pub show_sidebar: bool,
    pub status_message: Option<String>,

    pub permission_mode: PermissionMode,
    pub model: String,
    pub workspace_path: String,
    pub pending_approval_tx: Option<oneshot::Sender<ApprovalDecision>>,

    /// Flag: user pressed Enter, app.rs should send the message
    pub pending_send: Option<String>,
}

impl AppState {
    pub async fn new() -> Result<Self> {
        let workspace_path = std::env::current_dir()?
            .to_string_lossy()
            .to_string();

        Ok(Self {
            tabs: vec![TabState::new()],
            active_tab: 0,
            focus: Focus::Input,
            mode: AppMode::Normal,
            input_buffer: String::new(),
            cursor_position: 0,
            show_sidebar: false,
            status_message: Some("Welcome to rustain. Type a message to begin.".to_string()),
            permission_mode: PermissionMode::Normal,
            model: "claude-sonnet-4-6".to_string(),
            workspace_path,
            pending_approval_tx: None,
            pending_send: None,
        })
    }

    pub fn active_tab(&self) -> &TabState {
        &self.tabs[self.active_tab]
    }

    pub fn active_tab_mut(&mut self) -> &mut TabState {
        &mut self.tabs[self.active_tab]
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        match &self.mode {
            AppMode::Normal => self.handle_normal_key(key),
            _ => {} // Other modes handled in app.rs
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) {
        match self.focus {
            Focus::Input => self.handle_input_key(key),
            Focus::Chat => self.handle_chat_key(key),
            _ => {}
        }
    }

    fn handle_input_key(&mut self, key: KeyEvent) {
        match (key.modifiers, key.code) {
            // Send message
            (KeyModifiers::NONE, KeyCode::Enter) => {
                let input = self.input_buffer.trim().to_string();
                if !input.is_empty() && !self.active_tab().is_streaming {
                    // Add user message to conversation
                    let tab = self.active_tab_mut();
                    let conv = tab.ensure_conversation();
                    conv.messages.push(ChatMessage::user(input.clone()));
                    conv.updated_at = chrono::Utc::now().timestamp_millis();

                    self.input_buffer.clear();
                    self.cursor_position = 0;
                    self.pending_send = Some(input);
                    self.status_message = Some("Sending...".to_string());
                }
            }
            // Typing
            (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(c)) => {
                self.input_buffer.insert(self.cursor_position, c);
                self.cursor_position += 1;
            }
            // Backspace
            (KeyModifiers::NONE, KeyCode::Backspace) => {
                if self.cursor_position > 0 {
                    self.cursor_position -= 1;
                    self.input_buffer.remove(self.cursor_position);
                }
            }
            // Cursor movement
            (KeyModifiers::NONE, KeyCode::Left) => {
                self.cursor_position = self.cursor_position.saturating_sub(1);
            }
            (KeyModifiers::NONE, KeyCode::Right) => {
                if self.cursor_position < self.input_buffer.len() {
                    self.cursor_position += 1;
                }
            }
            // Escape: switch focus to chat
            (KeyModifiers::NONE, KeyCode::Esc) => {
                self.focus = Focus::Chat;
            }
            _ => {}
        }
    }

    fn handle_chat_key(&mut self, key: KeyEvent) {
        match key.code {
            // Vim-style scroll
            KeyCode::Char('j') | KeyCode::Down => {
                let tab = self.active_tab_mut();
                tab.scroll_offset = tab.scroll_offset.saturating_add(1);
                tab.auto_scroll = false;
            }
            KeyCode::Char('k') | KeyCode::Up => {
                let tab = self.active_tab_mut();
                tab.scroll_offset = tab.scroll_offset.saturating_sub(1);
            }
            // Focus input
            KeyCode::Char('i') => {
                self.focus = Focus::Input;
            }
            _ => {}
        }
    }

    pub fn handle_stream_event(&mut self, event: TuiStreamEvent) {
        // Handle events that only touch self.status_message first (no tab borrow)
        match &event {
            TuiStreamEvent::Error { content } => {
                self.active_tab_mut().is_streaming = false;
                self.status_message = Some(format!("Error: {}", content));
                return;
            }
            TuiStreamEvent::Done => {
                self.active_tab_mut().is_streaming = false;
                self.status_message = Some("Ready".to_string());
                return;
            }
            TuiStreamEvent::Usage {
                input_tokens,
                output_tokens,
                ..
            } => {
                self.status_message = Some(format!(
                    "tokens: {}in/{}out",
                    input_tokens, output_tokens
                ));
                return;
            }
            _ => {}
        }

        let tab = self.active_tab_mut();

        match event {
            TuiStreamEvent::Text { content, .. } => {
                tab.is_streaming = true;
                let conv = tab.ensure_conversation();

                let needs_new = conv.messages.last().map_or(true, |m| {
                    m.role != super::conversation::MessageRole::Assistant
                });
                if needs_new {
                    conv.messages.push(ChatMessage::assistant());
                }

                if let Some(msg) = conv.messages.last_mut() {
                    msg.content.push_str(&content);

                    let append_to_existing = matches!(
                        msg.content_blocks.last(),
                        Some(ContentBlock::Text { .. })
                    );
                    if append_to_existing {
                        if let Some(ContentBlock::Text { content: c }) =
                            msg.content_blocks.last_mut()
                        {
                            c.push_str(&content);
                        }
                    } else {
                        msg.content_blocks.push(ContentBlock::Text { content });
                    }
                }
            }

            TuiStreamEvent::Thinking { content, .. } => {
                let conv = tab.ensure_conversation();
                if let Some(msg) = conv.messages.last_mut() {
                    let append_to_existing = matches!(
                        msg.content_blocks.last(),
                        Some(ContentBlock::Thinking { .. })
                    );
                    if append_to_existing {
                        if let Some(ContentBlock::Thinking { content: c, .. }) =
                            msg.content_blocks.last_mut()
                        {
                            c.push_str(&content);
                        }
                    } else {
                        msg.content_blocks.push(ContentBlock::Thinking {
                            content,
                            duration_seconds: None,
                            collapsed: true,
                        });
                    }
                }
            }

            TuiStreamEvent::ToolUse { id, name, .. } => {
                let conv = tab.ensure_conversation();
                if let Some(msg) = conv.messages.last_mut() {
                    msg.content_blocks.push(ContentBlock::ToolUse {
                        tool_id: id,
                        name,
                        input: String::new(),
                        result: None,
                        is_error: false,
                        collapsed: false,
                    });
                }
            }

            TuiStreamEvent::ToolResult {
                id,
                content,
                is_error,
            } => {
                let conv = tab.ensure_conversation();
                if let Some(msg) = conv.messages.last_mut() {
                    for block in &mut msg.content_blocks {
                        if let ContentBlock::ToolUse {
                            tool_id,
                            result,
                            is_error: err,
                            collapsed,
                            ..
                        } = block
                        {
                            if *tool_id == id {
                                *result = Some(content.clone());
                                *err = is_error;
                                *collapsed = true;
                                break;
                            }
                        }
                    }
                }
            }

            _ => {} // Error, Done, Usage handled above
        }
    }
}
