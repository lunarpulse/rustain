//! State of the sign-in overlay: provider picker, method picker and the
//! running login (browser link, device code, progress, one pending field).

use crate::domain::events::{LoginReply, LoginTargetRow};
use crate::domain::models::{AuthMethod, FocusState, SecretString};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

/// Typed field text. `Debug` never shows it; the buffer is zeroed when taken,
/// cleared or dropped.
#[derive(Default)]
pub struct SecretField(Zeroizing<String>);

impl std::fmt::Debug for SecretField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretField(<redacted>)")
    }
}

impl SecretField {
    pub fn push(&mut self, c: char) {
        self.0.push(c);
    }
    pub fn push_str(&mut self, text: &str) {
        self.0.push_str(text);
    }
    pub fn pop(&mut self) {
        self.0.pop();
    }
    /// The only property a masked field reveals.
    pub fn char_count(&self) -> usize {
        self.0.chars().count()
    }
    /// Visible text of a non-secret (`prompt_line`) field.
    pub(crate) fn visible(&self) -> &str {
        &self.0
    }
    pub fn clear(&mut self) {
        // `Zeroizing` wipes the old allocation when it is replaced.
        self.0 = Zeroizing::new(String::new());
    }
    fn take(&mut self) -> String {
        std::mem::take(&mut *self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Validation {
    Checking,
    Valid,
    Invalid,
}

#[derive(Debug, Clone)]
pub struct PickerRow {
    pub row: LoginTargetRow,
    pub validation: Validation,
}

/// Where a login was started; a picker-origin login reopens the model picker
/// at that column whatever the outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginOrigin {
    Slash,
    Chord,
    Picker { column: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginView {
    ProviderPicker,
    MethodPicker,
    Running,
}

/// A link or code section of the running view, in arrival order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginSection {
    AuthUrl {
        url: String,
        launch_url: String,
        instructions: String,
    },
    Device {
        verification_url: String,
        user_code: String,
        instructions: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    /// `prompt_secret`: an API key. Masked.
    Secret,
    /// `manual_input`: an authorization code, redirect URL or pasted key. Masked.
    Manual,
    /// `prompt_line`: non-secret by contract. Visible.
    Line,
}

#[derive(Debug)]
enum FieldReply {
    Secret(LoginReply<Option<SecretString>>),
    Text(LoginReply<Option<String>>),
}

#[derive(Debug)]
pub struct PendingField {
    pub kind: FieldKind,
    pub prompt: String,
    pub buffer: SecretField,
    reply: FieldReply,
}

impl PendingField {
    pub fn secret(prompt: String, reply: LoginReply<Option<SecretString>>) -> Self {
        Self {
            kind: FieldKind::Secret,
            prompt,
            buffer: SecretField::default(),
            reply: FieldReply::Secret(reply),
        }
    }
    pub fn text(kind: FieldKind, prompt: String, reply: LoginReply<Option<String>>) -> Self {
        Self {
            kind,
            prompt,
            buffer: SecretField::default(),
            reply: FieldReply::Text(reply),
        }
    }
    /// Moves the typed text into the engine's reply; the buffer is left empty.
    pub fn submit(mut self) {
        let value = self.buffer.take();
        match self.reply {
            FieldReply::Secret(reply) => {
                let _ = reply.0.send(Some(SecretString::from(value)));
            }
            FieldReply::Text(reply) => {
                let _ = reply.0.send(Some(value));
            }
        }
    }
}

#[derive(Debug)]
pub struct RunningLogin {
    pub login_id: u64,
    pub provider_id: String,
    pub display: String,
    pub origin: LoginOrigin,
    pub cancel: CancellationToken,
    pub sections: Vec<LoginSection>,
    pub progress: Vec<String>,
    pub field: Option<PendingField>,
}

#[derive(Debug)]
pub struct LoginOverlayState {
    pub active: bool,
    pub view: LoginView,
    pub previous_focus: Option<FocusState>,
    pub origin: LoginOrigin,
    /// Discards picker data and validation results of a closed or reopened picker.
    pub generation: u64,
    pub rows: Vec<PickerRow>,
    pub loading: bool,
    pub query: String,
    pub filtered: Vec<usize>,
    pub selected: usize,
    /// The provider the method picker is for: (store key, display name).
    pub target: Option<(String, String)>,
    pub methods: &'static [AuthMethod],
    pub method_selected: usize,
    /// Esc in the method picker returns to the provider picker.
    pub method_from_picker: bool,
    pub running: Option<RunningLogin>,
    last_login_id: u64,
}

impl Default for LoginOverlayState {
    fn default() -> Self {
        Self {
            active: false,
            view: LoginView::ProviderPicker,
            previous_focus: None,
            origin: LoginOrigin::Slash,
            generation: 0,
            rows: Vec::new(),
            loading: false,
            query: String::new(),
            filtered: Vec::new(),
            selected: 0,
            target: None,
            methods: &[],
            method_selected: 0,
            method_from_picker: false,
            running: None,
            last_login_id: 0,
        }
    }
}

/// Rows the picker shows at once.
pub const LOGIN_PICKER_ROWS: usize = 10;

impl LoginOverlayState {
    pub fn next_login_id(&mut self) -> u64 {
        self.last_login_id += 1;
        self.last_login_id
    }

    /// Opens the provider picker in its loading state; returns the generation
    /// its data and validation results must carry.
    pub fn open_picker(&mut self, previous: FocusState, origin: LoginOrigin) -> u64 {
        self.generation += 1;
        self.active = true;
        self.view = LoginView::ProviderPicker;
        self.previous_focus.get_or_insert(previous);
        self.origin = origin;
        self.rows.clear();
        self.loading = true;
        self.query.clear();
        self.filtered.clear();
        self.selected = 0;
        self.generation
    }

    pub fn set_rows(&mut self, generation: u64, rows: Vec<LoginTargetRow>) {
        if generation != self.generation || !self.active {
            return;
        }
        self.loading = false;
        self.rows = rows
            .into_iter()
            .map(|row| PickerRow {
                validation: Validation::Checking,
                row,
            })
            .collect();
        self.refilter();
    }

    pub fn set_validation(&mut self, generation: u64, provider: &str, valid: bool) {
        if generation != self.generation {
            return;
        }
        if let Some(row) = self.rows.iter_mut().find(|row| row.row.id == provider) {
            row.validation = if valid {
                Validation::Valid
            } else {
                Validation::Invalid
            };
        }
    }

    /// Recomputes the visible rows: every row in order for an empty query,
    /// otherwise matches by score (palette scoring over name and id, source
    /// and env variable), ties in table order.
    pub fn refilter(&mut self) {
        let query = self.query.to_lowercase();
        let mut scored: Vec<(usize, u32)> = self
            .rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| {
                if query.is_empty() {
                    return Some((index, 1));
                }
                let score = crate::adapters::palette_registry::fuzzy_score(
                    &row.row.display,
                    &row.row.search,
                    &query,
                );
                (score > 0).then_some((index, score))
            })
            .collect();
        scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        self.filtered = scored.into_iter().map(|(index, _)| index).collect();
        self.selected = 0;
    }

    pub fn selected_row(&self) -> Option<&PickerRow> {
        self.filtered
            .get(self.selected)
            .and_then(|index| self.rows.get(*index))
    }

    pub fn open_methods(
        &mut self,
        previous: FocusState,
        origin: LoginOrigin,
        target: (String, String),
        methods: &'static [AuthMethod],
        from_picker: bool,
    ) {
        self.active = true;
        self.previous_focus.get_or_insert(previous);
        self.origin = origin;
        self.view = LoginView::MethodPicker;
        self.target = Some(target);
        self.methods = methods;
        self.method_selected = 0;
        self.method_from_picker = from_picker;
    }

    pub fn start_running(&mut self, running: RunningLogin) {
        self.active = true;
        self.view = LoginView::Running;
        self.running = Some(running);
    }

    /// The running login with this id, if it is still the current one.
    pub fn running_mut(&mut self, login_id: u64) -> Option<&mut RunningLogin> {
        self.running
            .as_mut()
            .filter(|running| running.login_id == login_id)
    }

    /// Cancels the running login (its late callbacks are then ignored).
    pub fn cancel_running(&mut self) {
        if let Some(running) = &mut self.running {
            running.cancel.cancel();
            if let Some(field) = &mut running.field {
                field.buffer.clear();
            }
            running.field = None;
        }
    }

    /// Closes the overlay; returns the focus to restore.
    pub fn close(&mut self) -> Option<FocusState> {
        self.active = false;
        self.generation += 1;
        self.rows.clear();
        self.query.clear();
        self.filtered.clear();
        self.target = None;
        self.previous_focus.take()
    }
}
