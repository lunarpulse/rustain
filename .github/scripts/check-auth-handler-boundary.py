#!/usr/bin/env python3
"""Fail-closed, bounded source ratchet for the TUI authentication spawn boundary.

This is deliberately not a Rust effect system. Planning functions have a finite
set of executable edges; changes to that set require reviewing this checker.

The sealed scope is PATHS, not arbitrary Rust dependencies. PLANNING names the
login/accounts entry points and their existing local planning/state helpers.
IMPORT_MODULES permits only existing auth/UI/domain references; runtime imports
are restricted to reviewed data types, never runtime functions or module aliases.
FREE_CALLS admits local planning helpers, host::plan/keyless_line, clipboard
flash_copy, model-selector opening, deferred-permission handoff and bounded text.
METHODS plus RECEIVERS seal the exact existing state/Option/string operations;
event/key handlers have their own finite synchronous edges. Nested imports,
callable bindings/parameters and macro definitions cannot shadow approved names.
AuthWork fields and the fieldless Change tag have exact reviewed data shapes.
Workers have finite import/call/receiver/collaborator surfaces; launch primitives
are rejected including generic calls and first-class values. Only cfg(test)
proves an item test-only. Separately reviewed completion edges admit only
run_login's directly awaited target.refresh_order.checkpoint calls, with the
exact cfg(any(test, feature = "test-instrumentation")) statement gate and
BeforeCompletion/CompletionFinished tags, and report.with_current(&target, ||…)
with an inline, independently inspected callback. Neither grants callable/spawn
authority; with_current's non-spawning implementation remains a trusted leaf.
Each producer must immediately feed its own original binding into dispatch,
which supplies or awaits worker futures in spawned tasks.

Non-claims: this is not whole-program Rust effect/type/alias analysis, arbitrary
macro expansion, general conditional-compilation evaluation, or a runtime
liveness/ordering proof. Reviewed synchronous collaborators and worker service
leaves remain trusted boundaries; their internals are not recursively analyzed.
Unsupported source shapes fail closed and require explicit allowlist review.
Behavioral keystones separately prove login/accounts parity and validation order.
"""
import argparse
from importlib.metadata import PackageNotFoundError, version
from pathlib import Path
import re
import sys

PINS = {"tree-sitter": "0.25.2", "tree-sitter-rust": "0.24.2"}
PATHS = (
    "src/adapters/tui/handlers/login.rs",
    "src/adapters/tui/handlers/accounts.rs",
    "src/adapters/tui/handlers/mod.rs",
    "src/adapters/tui/login_runtime.rs",
    "src/adapters/tui/accounts_runtime.rs",
    "src/infrastructure/runtime/event_loop.rs",
)


class BoundaryError(Exception):
    pass


def parser():
    try:
        import tree_sitter
        import tree_sitter_rust
        versions = {name: version(name) for name in PINS}
        if versions != PINS:
            raise BoundaryError(f"parser configuration mismatch: require {PINS}; found {versions}")
        return tree_sitter.Parser(tree_sitter.Language(tree_sitter_rust.language()))
    except (ImportError, PackageNotFoundError, TypeError, ValueError, RuntimeError) as error:
        raise BoundaryError(f"parser unavailable: {error}") from error


def walk(node):
    yield node
    for child in node.named_children:
        yield from walk(child)


def text(node):
    return node.text.decode("utf-8") if node is not None else ""


def fail(path, node, message):
    line = node.start_point.row + 1 if node is not None else 1
    raise BoundaryError(f"{path}:{line}: {message}")


def parse(p, path, source):
    try:
        source.decode("utf-8")
    except UnicodeDecodeError as error:
        raise BoundaryError(f"{path}: invalid UTF-8: {error}") from error
    root = p.parse(source).root_node
    if root.has_error:
        fail(path, root, "Rust parse failed")
    return root


def production(root):
    # Only an actual cfg(test) attribute proves an item is test-only. A module
    # merely named `tests` remains an ordinary, inspected production item.
    attributes = []
    for node in root.named_children:
        if node.type == "attribute_item":
            attributes.append(node)
            continue
        if not any(re.sub(r"\s+", "", text(attr)) == "#[cfg(test)]" for attr in attributes):
            yield from attributes
            yield node
        attributes.clear()


def production_nodes(root):
    for node in production(root):
        yield node
        yield from production_nodes(node)


def callee(node):
    """Unwrap Rust turbofish/parentheses without losing the actual value path."""
    while node is not None and node.type in {"generic_function", "parenthesized_expression"}:
        node = node.child_by_field_name("function") or next(iter(node.named_children), None)
    return node


def callable_name(node):
    node = callee(node)
    if node is None:
        return ""
    return text(node.child_by_field_name("field") or node.child_by_field_name("name") or node)


# Imports may name only these already-reviewed modules. Calls are independently
# checked below, so importing a type from one does not authorize its functions.
IMPORT_MODULES = (
    "crate::adapters::auth::host", "crate::adapters::tui::app",
    "crate::adapters::tui::login_runtime", "crate::adapters::tui::accounts_runtime",
    "crate::adapters::tui::login_ui", "crate::adapters::tui::state",
    "crate::domain::events", "crate::domain::models", "crate::domain::ports",
    "super::login", "super",
)
PLANNING = {
    "login.rs": {"command", "on_action", "open", "begin", "start", "take_focus", "close", "feedback", "failed_to_start", "display_text"},
    "accounts.rs": {"command", "on_action"},
}
FREE_CALLS = {
    "open", "begin", "start", "take_focus", "close", "feedback", "failed_to_start", "display_text",
    "host::plan", "host::keyless_line", "super::accounts::on_action",
    "super::clipboard::flash_copy", "super::model_switch::open_selector",
    "crate::adapters::tui::app::advance_deferred_permission",
    "crate::adapters::auth::bounded_provider_text", "str::trim", "str::to_owned",
    "Some", "Ok", "Err", "Vec::new", "FocusState::Overlay",
    "HandlerOutcome::RequestSpawn", "SpawnRequest::Authentication",
}
METHODS = {
    "map", "filter", "is_empty", "is_some", "clone", "get", "to_string", "to_owned",
    "dismiss", "unwrap_or", "open_picker", "open_accounts", "open_methods", "close",
    "get_or_insert", "next_login_id", "insert", "into", "as_deref",
}
RECEIVERS = {
    "map": {"args", "args.map(str::trim).filter(|arg|!arg.is_empty())"},
    "filter": {"args.map(str::trim)"},
    "is_empty": {"arg"},
    "is_some": {"state.login.running"},
    "clone": {"state.login.origin", "state.focus", "plan.provider_id", "plan.display", "provider", "block.id"},
    "get": {"plan.methods"},
    "to_string": {"error"},
    "to_owned": {"provider"},
    "dismiss": {"state.model_selector"},
    "unwrap_or": {"state.model_selector.dismiss()", "previous"},
    "open_picker": {"state.login"},
    "open_accounts": {"state.login"},
    "open_methods": {"state.login"},
    "close": {"state.login"},
    "get_or_insert": {"state.login.previous_focus"},
    "next_login_id": {"state.login"},
    "insert": {"state.feedback_blocks"},
    "into": {'"login-result"'},
}
MACROS = {"format"}

# Reviewed event/key edges are deliberately finite, including their receivers.
EVENT_FUNCTIONS = {
    "login.rs": {"on_event", "finished", "copy_target", "copy_key", "on_key",
                 "is_cancel", "move_cursor", "closed", "picker_key", "method_key", "running_key"},
    "accounts.rs": {"feedback", "nothing_stored", "on_event", "on_key"},
}
EVENT_FREE_CALLS = {
    "Some", "Ok", "Err", "Vec::new", "close", "feedback", "nothing_stored",
    "finished", "safe_url", "copy_key", "copy_target", "picker_key", "method_key",
    "running_key", "is_cancel", "closed", "move_cursor", "current",
    "super::accounts::on_event", "super::accounts::on_key",
    "super::clipboard::flash", "super::model_switch::open_selector", "host::login_targets",
    "PendingField::secret", "PendingField::text", "String::new",
    "InputAction::Login", "LoginAction::Copy", "LoginAction::Accounts",
}
EVENT_METHOD_TARGETS = {
    "state.login.set_rows", "state.login.set_validation", "state.login.running_mut",
    "running.sections.push", "running.progress.push", "state.login.running.as_ref",
    "state.login.running.as_ref().map", "state.login.running.take", "running.map",
    "summary.lines.join", "picker_column.unwrap_or_else", "host::login_targets(app).into_iter",
    "host::login_targets(app).into_iter().find",
    "host::login_targets(app).into_iter().find(|target|target.id==provider).and_then",
    "target.entries.into_iter", "target.entries.into_iter().next",
    "host::login_targets(app).into_iter().find(|target|target.id==provider).and_then(|target|target.entries.into_iter().next()).unwrap_or_else",
    "provider.to_owned", "state.login.running.as_ref().and_then", "running.sections.last",
    "user_code.is_empty", "user_code.chars", "user_code.chars().any",
    "(!user_code.is_empty()&&!user_code.chars().any(char::is_control)).then",
    "user_code.clone", "target.unwrap_or_default", "selected.saturating_sub",
    "(selected+LOGIN_PICKER_ROWS).min", "state.login.origin.clone",
    "login.query.is_empty", "login.query.clear", "login.refilter", "c.is_control",
    "login.query.push", "login.query.extend", "text.chars", "text.chars().filter",
    "login.query.pop", "login.selected_row", "row.row.id.clone", "login.filtered.len",
    "provider.clone", "login.methods.len", "state.login.cancel_running",
    "state.login.running.as_ref().is_some_and", "running.field.is_some",
    "state.login.running.as_mut", "running.field.as_mut", "field.buffer.push",
    "field.buffer.pop", "running.field.take", "field.submit", '"accounts-result".into',
    "block.id.clone", "state.feedback_blocks.insert", "rows.is_empty",
    "nothing_stored(mode).into", "state.login.set_accounts", "lines.join",
    "state.login.account.as_mut", "state.login.back_to_picker", "listing.rows.get",
    "listing.provider.clone", "listing.rows.len", '"Logincancelled".into',
}
EVENT_CALLBACKS = {
    "map": {"(|r|r.login_id)", "(|running|running.origin)"},
    "and_then": {"(|target|target.entries.into_iter().next())", "(|running|running.sections.last())"},
    "find": {"(|target|target.id==provider)"},
    "unwrap_or_else": {"(||{host::login_targets(app).into_iter().find(|target|target.id==provider).and_then(|target|target.entries.into_iter().next()).unwrap_or_else(||provider.to_owned())})", "(||provider.to_owned())"},
    "any": {"(char::is_control)"}, "then": {"(||user_code.clone())"},
    "filter": {"(|c|!c.is_control())"},
    "is_some_and": {"(|running|running.field.is_some())"},
}
EVENT_CLOSURES = {
    "|r|r.login_id", "|running|running.origin", "|target|target.id==provider",
    "|target|target.entries.into_iter().next()", "||provider.to_owned()",
    "||{host::login_targets(app).into_iter().find(|target|target.id==provider).and_then(|target|target.entries.into_iter().next()).unwrap_or_else(||provider.to_owned())}",
    "|running|running.sections.last()", "||user_code.clone()", "|c|!c.is_control()",
    "|running|running.field.is_some()",
    "|state:&TuiState,generation:u64|{generation==state.login.generation&&state.login.active}",
}


def check_imports(path, root):
    for item in production_nodes(root):
        if item.type == "macro_definition":
            fail(path, item, "handler macro definition may shadow an approved macro")
        if item.type != "use_declaration":
            continue
        if item.parent != root:
            fail(path, item, "nested handler import is unapproved")
        value = re.sub(r"\s+", "", text(item))[3:].rstrip(";")
        if "*" in value or re.search(r"\bas\b", text(item)):
            fail(path, item, "unapproved alias or glob import")
        if not any(value == module or value.startswith(module + "::") for module in IMPORT_MODULES):
            fail(path, item, f"unapproved helper import {value}")
        leaves = re.findall(r"(?:^|[,{}])([a-z_][a-z_0-9]*)(?=[,}]|$)", value)
        terminal = re.search(r"::([a-z_][a-z_0-9]*)$", value)
        if terminal:
            leaves.append(terminal.group(1))
        allowed_values = {"self", "host", "safe_url", "close", "closed", "is_cancel", "move_cursor", "take_focus"}
        if any(leaf not in allowed_values for leaf in leaves):
            fail(path, item, f"unapproved value or module import {value}")
        # Runtime imports are type-only. Even a renamed executable edge is not
        # allowed to hide beside Change/Copier/ValidationCheck.
        for runtime, allowed in (("login_runtime", {"Copier", "ValidationCheck"}),
                                 ("accounts_runtime", {"Change"})):
            if f"::{runtime}::" in value:
                suffix = value.split(f"::{runtime}::", 1)[1].strip("{}")
                if not set(suffix.split(",")) <= allowed:
                    fail(path, item, f"runtime value import {value}")
        if value in {"crate::adapters::tui::login_runtime", "crate::adapters::tui::accounts_runtime"}:
            fail(path, item, f"runtime module import {value}")
    for node in production_nodes(root):
        if node.type == "macro_invocation":
            macro = text(node.child_by_field_name("macro"))
            if macro not in {"format", "matches"}:
                fail(path, node, f"unapproved handler macro {macro}")


def check_handlers(path, root):
    check_imports(path, root)
    name = Path(path).name
    functions = {text(n.child_by_field_name("name")): n for n in production(root) if n.type == "function_item"}
    for entry in ("command", "on_action"):
        if entry not in functions:
            fail(path, root, f"missing handler entry {entry}")
        result_type = functions[entry].child_by_field_name("return_type")
        if text(result_type) != "HandlerOutcome":
            fail(path, functions[entry], f"{entry} must return HandlerOutcome")
    approved_functions = PLANNING[name] | EVENT_FUNCTIONS[name]
    protected = {value for value in FREE_CALLS | EVENT_FREE_CALLS
                 if "::" not in value and value[:1].islower()} | {"host", "format", "matches"}
    for node in production_nodes(root):
        if node.type == "function_item":
            fn_name = text(node.child_by_field_name("name"))
            if node.parent != root or fn_name not in approved_functions:
                fail(path, node, f"unapproved handler function {fn_name}")
        elif node.type.endswith("_item"):
            item_name = text(node.child_by_field_name("name"))
            if item_name in protected:
                fail(path, node, f"shadowed handler callable {item_name}")
        if node.type in {"call_expression", "macro_invocation", "closure_expression", "async_block", "await_expression"}:
            parent = node.parent
            while parent is not None and parent.type != "function_item":
                parent = parent.parent
            if parent is None:
                const = node.parent
                while const is not None and const.type != "const_item":
                    const = const.parent
                if not (node.type == "call_expression" and const is not None and
                        text(const.child_by_field_name("name")) == "LOGIN" and
                        re.sub(r"\s+", "", text(node)) == "FocusState::Overlay(OverlayType::Login)"):
                    fail(path, node, "unapproved executable edge outside handler function")
        if node.type == "parameter":
            parameter_type = text(node.child_by_field_name("type"))
            if re.search(r"\b(?:fn|Fn|FnMut|FnOnce)\b", parameter_type):
                fail(path, node, "callable handler parameter is unapproved")
        if node.type in {"let_declaration", "parameter", "closure_parameters", "match_pattern", "for_expression"}:
            pattern = node.child_by_field_name("pattern")
            if pattern is None and node.type == "closure_parameters":
                pattern = node
            if pattern is not None:
                bindings = {text(n) for n in walk(pattern) if n.type == "identifier"}
                if bindings & protected:
                    if not (node.type == "let_declaration" and bindings == {"current"} and
                            name == "accounts.rs" and
                            re.sub(r"\s+", "", text(node.child_by_field_name("value"))) ==
                            "|state:&TuiState,generation:u64|{generation==state.login.generation&&state.login.active}"):
                        fail(path, node, f"shadowed handler callable {sorted(bindings & protected)}")
    found_request = False
    for fn_name, fn in functions.items():
        planning = fn_name in PLANNING[name]
        if "async" in text(fn).split("fn", 1)[0]:
            fail(path, fn, f"async handler function {fn_name}")
        for node in production_nodes(fn):
            if node.type in {"async_block", "await_expression"}:
                fail(path, node, f"executable handler expression in {fn_name}")
            if node.type == "closure_expression":
                expression = re.sub(r"\s+", "", text(node))
                allowed = {"|arg|!arg.is_empty()"} if planning else EVENT_CLOSURES
                if expression not in allowed:
                    fail(path, node, f"unapproved callback in {fn_name}")
            if node.type == "macro_invocation":
                macro = text(node.child_by_field_name("macro"))
                expression = re.sub(r"\s+", "", text(node))
                allowed = {'format!("Loginfailed:{}",display_text(error))'} if planning else {
                    'format!("Loginfailed:{message}")',
                    "matches!(event,DomainInputEvent::SpecialKey(DomainKey::CtrlQ))",
                    "matches!(event,DomainInputEvent::SpecialKey(DomainKey::Esc|DomainKey::CtrlC))",
                }
                if expression not in allowed:
                    fail(path, node, f"unapproved macro {macro} in {fn_name}")
            if node.type != "call_expression":
                continue
            target = callee(node.child_by_field_name("function"))
            target_text = re.sub(r"\s+", "", text(target))
            if target_text == "HandlerOutcome::RequestSpawn":
                arguments = node.child_by_field_name("arguments")
                items = arguments.named_children if arguments is not None else []
                if len(items) == 1 and items[0].type == "call_expression":
                    nested = items[0].child_by_field_name("function")
                    if re.sub(r"\s+", "", text(nested)) == "SpawnRequest::Authentication":
                        found_request = True
            if not planning:
                if target is not None and target.type == "field_expression":
                    method = callable_name(target)
                    if target_text not in EVENT_METHOD_TARGETS:
                        fail(path, node, f"unapproved method edge {target_text} in {fn_name}")
                    if method in EVENT_CALLBACKS:
                        arguments = re.sub(r"\s+", "", text(node.child_by_field_name("arguments")))
                        if arguments not in EVENT_CALLBACKS[method]:
                            fail(path, node, f"unapproved callback to {method} in {fn_name}")
                elif target_text not in EVENT_FREE_CALLS:
                    fail(path, node, f"unapproved executable edge {target_text} in {fn_name}")
                continue
            if target is not None and target.type == "field_expression":
                method = text(target.child_by_field_name("field"))
                if method not in METHODS:
                    fail(path, node, f"unapproved method {method} in {fn_name}")
                if method in {"map", "filter"}:
                    arguments = re.sub(r"\s+", "", text(node.child_by_field_name("arguments")))
                    allowed = {"(str::trim)", "(str::to_owned)"} if method == "map" else {"(|arg|!arg.is_empty())"}
                    if arguments not in allowed:
                        fail(path, node, f"unapproved callback to {method} in {fn_name}")
                receiver = re.sub(r"\s+", "", text(target.child_by_field_name("value")))
                if receiver not in RECEIVERS.get(method, set()):
                    fail(path, node, f"unapproved receiver {receiver}.{method} in {fn_name}")
            elif target_text not in FREE_CALLS:
                fail(path, node, f"unapproved executable edge {target_text} in {fn_name}")
    if not found_request:
        fail(path, root, "missing authentication outcome-producing route")


def check_payload(path, root):
    enums = {text(n.child_by_field_name("name")): n for n in root.named_children if n.type == "enum_item"}
    if "AuthWork" not in enums or "SpawnRequest" not in enums:
        fail(path, root, "missing AuthWork or SpawnRequest")
    auth = enums["AuthWork"]
    if not re.search(r"\bAuthentication\s*\(\s*AuthWork\s*\)", text(enums["SpawnRequest"])):
        fail(path, enums["SpawnRequest"], "SpawnRequest missing Authentication(AuthWork)")
    allowed_types = {
        "u64", "String", "crate::adapters::auth::host::LoginPlan",
        "&'staticcrate::domain::models::AuthMethod",
        "crate::adapters::tui::state::LoginOrigin",
        "crate::adapters::tui::state::OverlayMode",
        "crate::adapters::tui::accounts_runtime::Change",
    }
    # The only unqualified non-primitive type is the prelude String. Reject
    # shadowing, including imports, rather than trusting an arbitrary alias.
    for item in root.named_children:
        name = text(item.child_by_field_name("name"))
        if name == "String" or (item.type == "use_declaration" and
                                re.search(r"\bString\b", text(item))):
            fail(path, item, "AuthWork String type is shadowed")
    for field in (n for n in walk(auth) if n.type == "field_declaration"):
        field_type = field.child_by_field_name("type")
        if field_type is None:
            fail(path, field, "AuthWork field missing type")
        spelling = re.sub(r"\s+", "", text(field_type))
        if spelling not in allowed_types:
            fail(path, field, f"AuthWork unapproved payload type {spelling}")
    if any(n.type == "ordered_field_declaration_list" for n in walk(auth)):
        fail(path, auth, "AuthWork tuple payload is unapproved")
    variants = {text(n.child_by_field_name("name")) for n in walk(auth) if n.type == "enum_variant"}
    if variants != {"Login", "Targets", "Providers", "Listing", "Change"}:
        fail(path, auth, f"AuthWork variant mismatch: {sorted(variants)}")


def check_change(path, root):
    changes = [item for item in production(root)
               if item.type == "enum_item" and text(item.child_by_field_name("name")) == "Change"]
    if len(changes) != 1:
        fail(path, root, "missing or duplicate accounts Change enum")
    change = changes[0]
    variants = [node for node in walk(change) if node.type == "enum_variant"]
    if len(variants) != 2 or {text(node) for node in variants} != {"Logout", "Select"}:
        fail(path, change, "accounts Change must contain exactly fieldless Logout and Select")


# These are the reviewed worker edges, not a transitive call-graph analysis.
WORKER_LAUNCHERS = {
    "spawn", "spawn_local", "spawn_blocking", "spawn_on", "spawn_local_on",
    "spawn_blocking_on",
}
WORKER_IMPORTS = {
    "usecrate::adapters::auth::flows::{self,FlowCtx,FlowError,Sleeper};",
    "usecrate::adapters::auth::host::{self,LoginPlan};",
    "usecrate::adapters::auth::host;",
    "usecrate::adapters::tui::clipboard::ClipboardResult;",
    "usecrate::adapters::tui::handlers::login::display_text;",
    "usecrate::adapters::tui::login_ui::{Opener,TuiLoginUi};",
    "usecrate::adapters::tui::login_runtime::LoginCtx;",
    "usecrate::adapters::tui::state::OverlayMode;",
    "usecrate::domain::events::{AppEvent,LoginEvent,LoginFailure,LoginSummary,LoginTargetRow};",
    "usecrate::domain::events::{AccountListRow,AccountListing,AppEvent,LoginEvent,LoginTargetRow};",
    "usecrate::domain::models::{AppConfig,AuthMethod};",
    "usecrate::domain::models::{AppConfig,CredentialId,FeedbackLevel};",
    "usecrate::domain::ports::AuthStorePort;",
    "usecrate::infrastructure::live_providers::LiveProviders;",
    "usecrate::infrastructure::runtime::event_bus::EventBus;",
    "usecrate::infrastructure::auth_accounts::{self,AccountError,AccountKind,ProviderAccounts,describe,};",
    "usestd::sync::Arc;",
}
WORKER_CALLS = {
    "Arc::new", "Some", "Ok", "Err", "CredentialId", "display_text", "emit",
    "unreadable", "listing", "failed", "closing", "describe", "FlowCtx::new",
    "AppEvent::Login", "LoginFailure::Failed", "flows::login",
    "host::success_line", "host::credentials_saved_line", "host::active_line",
    "host::discovery_line", "host::login_targets", "host::source_label",
    "host::status_text", "host::account_header", "host::row_state",
    "host::stored_accounts", "host::known_provider", "host::logout_lines",
    "host::select_lines", "auth_accounts::logout_account",
    "auth_accounts::select_account", "auth_accounts::accounts", "Vec::new",
    "crate::adapters::auth_store::FileAuthStore::new",
    "crate::adapters::auth_store::FileAuthStore::try_snapshot_sync",
    "crate::infrastructure::auth_accounts::accounts",
    "crate::infrastructure::auth_accounts::describe",
    "crate::infrastructure::provider_availability::effective_providers",
    "crate::infrastructure::key_resolver::CredentialHandle::new",
    "crate::infrastructure::key_resolver::shared",
}
WORKER_VALUES = {
    "crate::adapters::auth::opener::open_url",
    "crate::adapters::tui::clipboard::copy_to_clipboard",
    "crate::infrastructure::utils::auth_now_ms",
    "crate::infrastructure::utils::env_var_trimmed", "str::to_owned",
}
WORKER_METHODS = {
    "emit_domain", "clone", "with_validation_config", "with_sleeper", "to_string",
    "emit", "register_change", "push", "iter", "filter", "is_none", "count",
    "refresh_changed", "snapshot", "unwrap_or_default", "find", "rows_key",
    "map", "env_vars", "into_iter", "collect", "is_some", "row_policy",
    "as_deref", "unwrap_or", "join", "current", "is_ok", "sort_by", "cmp",
    "then_with", "trim_end", "to_owned", "is_empty", "row", "ok", "as_ref",
    "map_or", "as_str",
}
WORKER_RECEIVERS = {
    "self", "ctx", "flow", "other", "error", "accounts", "lines", "target",
    "entries", "entry", "env_vars", "source", "checks", "rows", "policy",
    "provider", "a", "b", "row", "fresh", "line", "result", "text", "snapshot",
    "crate", "FlowCtx", "sleeper", "host", "describe", "auth_accounts",
}
WORKER_PARAMETERS = {
    "new": {"(events:Arc<EventBus>,live:Arc<LiveProviders>)"},
    "with_opener": {"(mutself,opener:Opener)"},
    "with_copier": {"(mutself,copier:Copier)"},
    "with_clock": {"(mutself,clock:Arc<dynFn()->i64+Send+Sync>)"},
    "with_sleeper": {"(mutself,sleeper:Sleeper)"},
    "emit": {"(&self,event:LoginEvent)", "(ctx:&LoginCtx,event:LoginEvent)"},
    "run_login": {"(ctx:LoginCtx,app:AppConfig,login_id:u64,plan:LoginPlan,method:&'staticAuthMethod,ui:Arc<TuiLoginUi>)"},
    "prepare_targets": {"(ctx:LoginCtx,app:AppConfig,generation:u64)"},
    "validate_target": {"(ctx:LoginCtx,generation:u64,check:ValidationCheck)"},
    "unreadable": {"(generation:u64,error:&dynstd::fmt::Display)"},
    "listing": {"(accounts:&ProviderAccounts)"},
    "run_providers": {"(ctx:LoginCtx,app:AppConfig,generation:u64)"},
    "run_listing": {"(ctx:LoginCtx,app:AppConfig,generation:u64,mode:OverlayMode,provider:String)"},
    "run_change": {"(ctx:LoginCtx,app:AppConfig,generation:u64,change:Change,provider:String,id:u64)"},
    "failed": {"(ctx:&LoginCtx,app:&AppConfig,generation:u64,provider:&str,error:AccountError)"},
}


def worker_macro_text(node):
    if node.type in {"string_literal", "raw_string_literal", "line_comment", "block_comment"}:
        return ""
    if not node.children:
        return text(node)
    return "".join(worker_macro_text(child) for child in node.children)


def reviewed_completion_wait(path, node):
    """Admit exact non-spawning completion publication/instrumentation waits."""
    if path != PATHS[3] or node is None or node.type != "call_expression":
        return False
    target = node.child_by_field_name("function")
    if target is None or target.type != "field_expression":
        return False
    value = re.sub(r"\s+", "", text(target))
    arguments = node.child_by_field_name("arguments")
    if value == "target.refresh_order.checkpoint":
        if re.sub(r"\s+", "", text(arguments)) not in {
            "(crate::adapters::model_catalog_cache::RefreshCheckpoint::BeforeCompletion)",
            "(crate::adapters::model_catalog_cache::RefreshCheckpoint::CompletionFinished)",
        }:
            return False
    elif value == "report.with_current":
        args = arguments.named_children if arguments is not None else []
        if (len(args) != 2 or text(args[0]) != "&target"
                or args[1].type != "closure_expression"
                or text(args[1].child_by_field_name("parameters")) != "||"):
            return False
    else:
        return False
    awaited = node.parent
    statement = awaited.parent if awaited is not None else None
    if (awaited is None or awaited.type != "await_expression"
            or statement is None or statement.type != "expression_statement"):
        return False
    if value == "target.refresh_order.checkpoint":
        gate = statement.prev_named_sibling
        if (gate is None or gate.type != "attribute_item"
                or re.sub(r"\s+", "", text(gate))
                != '#[cfg(any(test,feature="test-instrumentation"))]'):
            return False
    function = next((a for a in ancestors(node) if a.type == "function_item"), None)
    return function is not None and text(function.child_by_field_name("name")) == "run_login"


def check_worker(path, root, required):
    nodes = list(production_nodes(root))
    functions = {text(n.child_by_field_name("name")): n for n in nodes if n.type == "function_item"}
    for name in required:
        if name not in functions or not re.search(r"\basync\s+fn\b", text(functions[name]).split("{", 1)[0]):
            fail(path, root, f"missing async worker {name}")
    # Diagnose launch intent before a preceding, independently unapproved setup
    # edge (e.g. JoinSet::new), so each primitive control is discriminating.
    for node in nodes:
        if node.type == "call_expression":
            target = callee(node.child_by_field_name("function"))
            if callable_name(target) in WORKER_LAUNCHERS:
                fail(path, node, f"task spawn in work module: {text(target)}")
        elif node.type in {"scoped_identifier", "identifier", "field_identifier"}:
            if callable_name(node) in WORKER_LAUNCHERS:
                fail(path, node, "task launcher value in work module")
        if node.type == "field_expression" and callable_name(node) in {"checkpoint", "with_current"}:
            call = node.parent
            if (call is None or call.type != "call_expression"
                    or call.child_by_field_name("function") != node
                    or not reviewed_completion_wait(path, call)):
                fail(path, node, "unapproved completion/checkpoint call or callable value")
    protected = {value for value in WORKER_CALLS if "::" not in value} | {"format", "vec"}
    for node in nodes:
        if node.type == "use_declaration":
            value = re.sub(r"\s+", "", text(node))
            if value not in WORKER_IMPORTS:
                fail(path, node, "unapproved worker import or task launcher")
        if node.type == "macro_definition":
            fail(path, node, "unapproved worker macro definition")
        if node.type.endswith("_item") and node.type != "function_item":
            if text(node.child_by_field_name("name")) in protected:
                fail(path, node, "shadowed worker callable item binding")
        if node.type in {"let_declaration", "parameter", "match_pattern", "closure_parameters"}:
            pattern = node.child_by_field_name("pattern")
            if pattern is None and node.type == "closure_parameters":
                pattern = node
            bindings = {text(n) for n in walk(pattern)} if pattern is not None else set()
            if bindings & protected and not (node.type == "let_declaration" and
                                             bindings == {"closing"}):
                fail(path, node, "shadowed worker callable binding")
        if node.type == "function_item":
            name = text(node.child_by_field_name("name"))
            parameters = node.child_by_field_name("parameters")
            signature = re.sub(r"\s+", "", text(parameters)).replace(",)", ")")
            if signature != "()" and signature not in WORKER_PARAMETERS.get(name, set()):
                fail(path, node, "unapproved worker callable parameters")
            if node.child_by_field_name("type_parameters") or node.child_by_field_name("where_clause"):
                fail(path, node, "unapproved worker generic callable surface")
        if node.type == "call_expression":
            target = callee(node.child_by_field_name("function"))
            name = callable_name(target)
            value = re.sub(r"\s+", "", text(target))
            if name in WORKER_LAUNCHERS:
                fail(path, node, f"task spawn in work module: {value}")
            if name in {"checkpoint", "with_current"} and reviewed_completion_wait(path, node):
                continue
            if target is not None and target.type == "field_expression":
                receiver = target.child_by_field_name("value")
                origin = re.match(r"\s*([A-Za-z_]\w*)", text(receiver))
                if name not in WORKER_METHODS or not origin or origin.group(1) not in WORKER_RECEIVERS:
                    fail(path, node, f"unapproved worker method receiver {value}")
                if name in {"map", "filter", "find", "sort_by", "then_with", "map_or"}:
                    arguments = node.child_by_field_name("arguments")
                    args = arguments.named_children if arguments else []
                    callback = args[-1] if args else None
                    if callback is not None and callback.type != "closure_expression" and text(callback) != "str::to_owned":
                        fail(path, callback, "unapproved worker callback")
            elif value not in WORKER_CALLS:
                fail(path, node, f"unapproved worker executable edge {value}")
        if node.type == "scoped_identifier":
            value = re.sub(r"\s+", "", text(node))
            name = callable_name(node)
            if name in WORKER_LAUNCHERS:
                fail(path, node, "task launcher value in work module")
            parent = node.parent
            if parent is not None and parent.type in {"scoped_identifier", "scoped_type_identifier"}:
                continue
            if any(a.type == "use_declaration" for a in ancestors(node)):
                continue
            if name and name[0].islower() and value not in WORKER_CALLS | WORKER_VALUES:
                fail(path, node, f"unapproved worker callable value {value}")
        if node.type == "let_declaration":
            value = node.child_by_field_name("value")
            if value is not None and value.type == "closure_expression":
                pattern = text(node.child_by_field_name("pattern"))
                if pattern != "closing" or not any(
                    a.type == "function_item" and text(a.child_by_field_name("name")) == "run_listing"
                    for a in ancestors(node)
                ):
                    fail(path, node, "unapproved worker callable binding")
        if node.type == "macro_invocation":
            if text(node.child_by_field_name("macro")) not in {"format", "vec"}:
                fail(path, node, "unapproved macro in work module")
            tokens = [n for n in walk(node) if n.type == "identifier"]
            if any(text(n) in WORKER_LAUNCHERS for n in tokens):
                fail(path, node, "task launcher inside work-module macro")
            if any(text(n) in {"checkpoint", "with_current"} for n in tokens):
                fail(path, node, "completion wait or callable inside work-module macro")
            # Token trees are opaque to tree-sitter: inspect executable paths
            # without interpreting string literals or comments as code.
            token_text = worker_macro_text(node)
            for match in re.finditer(r"(?:[A-Za-z_]\w*::)+[A-Za-z_]\w*", token_text):
                if match.group() not in WORKER_CALLS | WORKER_VALUES:
                    fail(path, node, f"unapproved worker macro edge {match.group()}")
            for match in re.finditer(r"\b([A-Za-z_]\w*)\s*\(", token_text):
                name = match.group(1)
                if name not in WORKER_METHODS and name not in {call.split("::")[-1] for call in WORKER_CALLS}:
                    fail(path, node, f"unapproved worker macro call {name}")


def masked(root, source):
    data = bytearray(source)
    for node in walk(root):
        if node.type in {"line_comment", "block_comment", "string_literal", "raw_string_literal", "char_literal"}:
            for i in range(node.start_byte, node.end_byte):
                if data[i] != 10:
                    data[i] = 32
    return bytes(data)


def ancestors(node):
    while node is not None:
        yield node
        node = node.parent


def expression(node):
    if node is not None and node.type == "expression_statement":
        return next(iter(node.named_children), None)
    return node


def statements(block):
    return [node for node in block.named_children
            if node.type not in {"line_comment", "block_comment"}]


def same_binding(node, name):
    return node is not None and node.type == "identifier" and text(node) == name


def auth_binding(pattern):
    match = re.fullmatch(
        r"HandlerOutcome::RequestSpawn\(SpawnRequest::Authentication\(([A-Za-z_]\w*)\)\)",
        re.sub(r"\s+", "", text(pattern)),
    )
    return match.group(1) if match else None


def interpreted_work(interpretation, variable):
    """Recognize only an immediate interpretation of the original let binding."""
    if interpretation.type == "if_expression":
        condition = interpretation.child_by_field_name("condition")
        if condition is None or condition.type != "let_condition":
            return None
        if not same_binding(condition.child_by_field_name("value"), variable):
            return None
        work = auth_binding(condition.child_by_field_name("pattern"))
        branch = interpretation.child_by_field_name("consequence")
    elif interpretation.type == "match_expression":
        if not same_binding(interpretation.child_by_field_name("value"), variable):
            return None
        body = interpretation.child_by_field_name("body")
        arms = [(arm, auth_binding(arm.child_by_field_name("pattern")))
                for arm in body.named_children if arm.type == "match_arm"]
        arms = [(arm, name) for arm, name in arms if name is not None]
        if len(arms) != 1:
            return None
        arm, work = arms[0]
        branch = arm.child_by_field_name("value")
    else:
        return None
    if work is None or branch is None:
        return None
    items = statements(branch) if branch.type == "block" else [branch]
    if not items:
        return None
    work_match = expression(items[-1])
    if (work_match.type != "match_expression"
            or not same_binding(work_match.child_by_field_name("value"), work)):
        return None
    # Prelude bindings may prepare ctx/app, never replace or consume the work.
    for prelude in items[:-1]:
        if any(n.type == "identifier" and text(n) in {variable, work} for n in walk(prelude)):
            return None
    for node in walk(branch):
        if node.type in {"let_declaration", "let_condition", "parameter"}:
            pattern = node.child_by_field_name("pattern")
            if pattern is not None and any(
                n.type == "identifier" and text(n) in {variable, work} for n in walk(pattern)
            ):
                return None
        if node.type in {"assignment_expression", "compound_assignment_expr"}:
            left = node.child_by_field_name("left")
            if left is not None and any(
                n.type == "identifier" and text(n) in {variable, work} for n in walk(left)
            ):
                return None
    return work_match


def direct_spawn(node):
    """Return the Tokio spawn taking this exact expression as its argument."""
    arguments = node.parent
    if arguments is None or arguments.type != "arguments" or len(arguments.named_children) != 1:
        return None
    parent = arguments.parent
    if parent is not None and parent.type == "call_expression":
        function = callee(parent.child_by_field_name("function"))
        if re.sub(r"\s+", "", text(function)) == "tokio::spawn":
            return parent
    return None


def execution_spawns(call, arm):
    # A worker is either supplied as the future to spawn, or actually awaited.
    # Merely nesting its construction inside a spawned future is not execution.
    if direct_spawn(call) is None and (call.parent is None or call.parent.type != "await_expression"):
        return None
    spawns = []
    for node in ancestors(call.parent):
        if node == arm:
            return spawns or None
        if node.type in {"closure_expression", "function_item", "if_expression", "match_expression"}:
            return None
        if node.type == "async_block" and direct_spawn(node) is None:
            return None
        if node.type == "call_expression" and re.sub(
            r"\s+", "", text(callee(node.child_by_field_name("function")))
        ) == "tokio::spawn":
            spawns.append(node)
    return None


def dispatched_workers(work_match):
    workers = {"Login": "run_login", "Targets": "prepare_targets",
               "Providers": "run_providers", "Listing": "run_listing", "Change": "run_change"}
    body = work_match.child_by_field_name("body")
    for variant, worker in workers.items():
        arms = [arm for arm in body.named_children if arm.type == "match_arm"
                and re.match(r"AuthWork\s*::\s*" + variant + r"\b",
                             text(arm.child_by_field_name("pattern")))]
        if len(arms) != 1 or re.search(r"\bif\b", text(arms[0].child_by_field_name("pattern"))):
            return False
        arm = arms[0]
        calls = [node for node in walk(arm) if node.type == "call_expression"]
        work = [node for node in calls if callable_name(node.child_by_field_name("function")) == worker]
        if len(work) != 1 or execution_spawns(work[0], arm) is None:
            return False
        if variant == "Targets":
            validations = [node for node in calls
                           if callable_name(node.child_by_field_name("function")) == "validate_target"]
            if len(validations) != 1:
                return False
            validation_spawns = execution_spawns(validations[0], arm)
            coordinator_spawns = execution_spawns(work[0], arm)
            if (validation_spawns is None or len(validation_spawns) != 2
                    or not any(spawn in validation_spawns for spawn in coordinator_spawns)):
                return False
    return True


def dispatch_region(p, path, fragment, producer):
    tree = p.parse(b"fn fixture() " + fragment).root_node
    if tree.has_error:
        return False
    for call in (n for n in walk(tree) if n.type == "call_expression"
                 and re.sub(r"\s+", "", text(callee(n.child_by_field_name("function")))) == producer):
        declaration = next((n for n in ancestors(call) if n.type == "let_declaration"), None)
        if declaration is None:
            continue
        between = []
        for node in ancestors(call):
            if node == declaration:
                break
            between.append(node)
        # Only a producer itself or the tail of a conditional is a work value.
        if any(n != call and n.type not in {"block", "if_expression", "else_clause"} for n in between):
            continue
        pattern = declaration.child_by_field_name("pattern")
        block = declaration.parent
        if pattern is None or pattern.type != "identifier" or block is None or block.type != "block":
            continue
        siblings = statements(block)
        index = siblings.index(declaration)
        if index + 1 == len(siblings):
            continue
        interpretation = expression(siblings[index + 1])
        work_match = interpreted_work(interpretation, text(pattern))
        if work_match is not None and dispatched_workers(work_match):
            return True
    return False


def check_dispatch(p, path, root, source):
    runs = [n for n in root.named_children if n.type == "function_item" and text(n.child_by_field_name("name")) == "run"]
    if len(runs) != 1:
        fail(path, root, "missing or ambiguous event-loop run")
    run = runs[0]
    code = masked(root, source)
    # The run contains unexpanded macro token trees. Balance braces in their
    # masked concrete syntax, then parse candidate blocks as standalone Rust.
    stack, pairs = [], []
    for i in range(run.start_byte, run.end_byte):
        if code[i] == 123:
            stack.append(i)
        elif code[i] == 125 and stack:
            pairs.append((stack.pop(), i + 1))
    for producer in ("handlers::login::command", "handlers::accounts::command", "handlers::login::on_action"):
        pattern = re.compile(rb"\b" + rb"\s*::\s*".join(part.encode() for part in producer.split("::")) + rb"\s*\(")
        occurrences = list(pattern.finditer(code, run.start_byte, run.end_byte))
        if len(occurrences) != 1:
            fail(path, run, f"{producer}: expected exactly one production producer")
        pos = occurrences[0].start()
        candidates = sorted((a, b) for a, b in pairs if a < pos < b)
        if not any(dispatch_region(p, path, source[a:b], producer) for a, b in reversed(candidates)):
            fail(path, run, f"{producer}: outcome does not feed its local dispatch-owned spawns")


def check(root):
    p = parser()
    parsed = []
    for path in PATHS:
        try:
            source = (Path(root) / path).read_bytes()
        except OSError as error:
            raise BoundaryError(f"{path}: source unavailable: {error}") from error
        parsed.append((parse(p, path, source), source))
    check_handlers(PATHS[0], parsed[0][0])
    check_handlers(PATHS[1], parsed[1][0])
    check_payload(PATHS[2], parsed[2][0])
    check_worker(PATHS[3], parsed[3][0], {"run_login", "prepare_targets", "validate_target"})
    check_worker(PATHS[4], parsed[4][0], {"run_providers", "run_listing", "run_change"})
    check_change(PATHS[4], parsed[4][0])
    check_dispatch(p, PATHS[5], *parsed[5])


def main():
    args = argparse.ArgumentParser(description=__doc__)
    args.add_argument("root", nargs="?", default=".")
    options = args.parse_args()
    try:
        check(options.root)
    except BoundaryError as error:
        print(error, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
