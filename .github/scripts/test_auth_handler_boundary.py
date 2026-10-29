#!/usr/bin/env python3
"""Deterministic controls for the production authentication boundary checker."""
import importlib.util
from pathlib import Path
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location("auth_boundary", Path(__file__).with_name("check-auth-handler-boundary.py"))
checker = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(checker)

LOGIN = '''use crate::adapters::tui::login_runtime::ValidationCheck;
fn command() -> HandlerOutcome { open() }
fn on_action() -> HandlerOutcome { open() }
fn open() -> HandlerOutcome { HandlerOutcome::RequestSpawn(SpawnRequest::Authentication(AuthWork::Targets { generation: 1 })) }
'''
ACCOUNTS = '''use crate::adapters::tui::accounts_runtime::Change;
fn command() -> HandlerOutcome { HandlerOutcome::RequestSpawn(SpawnRequest::Authentication(AuthWork::Providers { generation: 1 })) }
fn on_action() -> HandlerOutcome { HandlerOutcome::Quiet }
'''
PAYLOAD = '''enum SpawnRequest { Authentication(AuthWork) }
enum HandlerOutcome { Quiet, RequestSpawn(SpawnRequest) }
enum AuthWork { Login { login_id: u64 }, Targets { generation: u64 }, Providers { generation: u64 }, Listing { generation: u64 }, Change { generation: u64 } }
'''
LOGIN_WORK = '''async fn run_login() {} async fn prepare_targets() {} async fn validate_target() {}'''
ACCOUNT_WORK = '''enum Change { Logout, Select }
async fn run_providers() {} async fn run_listing() {} async fn run_change() {}'''
DISPATCH = '''async fn run() {
 let outcome = handlers::login::command();
 match outcome { HandlerOutcome::RequestSpawn(SpawnRequest::Authentication(work)) => match work {
 AuthWork::Login { .. } => { tokio::spawn(run_login()); },
 AuthWork::Targets { .. } => { tokio::spawn(async { prepare_targets().await; tokio::spawn(validate_target()); }); },
 AuthWork::Providers { .. } => { tokio::spawn(run_providers()); },
 AuthWork::Listing { .. } => { tokio::spawn(run_listing()); },
 AuthWork::Change { .. } => { tokio::spawn(run_change()); }, }, _ => {} }
 let outcome = handlers::accounts::command();
 match outcome { HandlerOutcome::RequestSpawn(SpawnRequest::Authentication(work)) => match work {
 AuthWork::Login { .. } => { tokio::spawn(run_login()); },
 AuthWork::Targets { .. } => { tokio::spawn(async { prepare_targets().await; tokio::spawn(validate_target()); }); },
 AuthWork::Providers { .. } => { tokio::spawn(run_providers()); },
 AuthWork::Listing { .. } => { tokio::spawn(run_listing()); },
 AuthWork::Change { .. } => { tokio::spawn(run_change()); }, }, _ => {} }
 let outcome = handlers::login::on_action();
 match outcome { HandlerOutcome::RequestSpawn(SpawnRequest::Authentication(work)) => match work {
 AuthWork::Login { .. } => { tokio::spawn(run_login()); },
 AuthWork::Targets { .. } => { tokio::spawn(async { prepare_targets().await; tokio::spawn(validate_target()); }); },
 AuthWork::Providers { .. } => { tokio::spawn(run_providers()); },
 AuthWork::Listing { .. } => { tokio::spawn(run_listing()); },
 AuthWork::Change { .. } => { tokio::spawn(run_change()); }, }, _ => {} }
}'''


class BoundaryControls(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.sources = dict(zip(checker.PATHS, (LOGIN, ACCOUNTS, PAYLOAD, LOGIN_WORK, ACCOUNT_WORK, DISPATCH)))

    def put(self):
        for path, source in self.sources.items():
            target = self.root / path
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(source, encoding="utf-8")

    def rejected(self, path, source, diagnostic):
        self.sources[path] = source
        self.put()
        with self.assertRaisesRegex(checker.BoundaryError, diagnostic):
            checker.check(self.root)

    def test_data_only_workers_and_dispatch(self):
        self.sources[checker.PATHS[0]] += '\n// tokio::spawn is just a comment\nconst NOTE: &str = "spawn";\n'
        self.put()
        checker.check(self.root)

    def test_comments_and_strings_cannot_forge_launch_route(self):
        source = '''fn command() -> HandlerOutcome { open() }
fn on_action() -> HandlerOutcome { open() }
fn open() -> HandlerOutcome {
    // HandlerOutcome::RequestSpawn(SpawnRequest::Authentication(...))
    let _ = "RequestSpawn Authentication";
    HandlerOutcome::Quiet
}
'''
        self.rejected(checker.PATHS[0], source, 'login.rs.*missing authentication outcome-producing route')

    def test_direct_and_wrapped_launches(self):
        for expression in ('tokio::spawn(work())', 'crate::adapters::tui::login_runtime::spawn_targets()'):
            with self.subTest(expression=expression):
                self.rejected(checker.PATHS[0], LOGIN.replace('open() }', expression + ' }', 1), 'login.rs')

    def test_baseline_indirection_has_no_handler_primitive(self):
        source = LOGIN.replace('open() }', 'crate::adapters::tui::login_runtime::spawn_targets() }', 1)
        self.assertNotIn('tokio::spawn', source)
        self.sources[checker.PATHS[3]] += '\nfn spawn_targets() { tokio::spawn(async {}); }\n'
        self.rejected(checker.PATHS[0], source, 'login.rs.*executable edge')

    def test_alias_and_new_helper(self):
        for injection in ('use crate::adapters::tui::login_runtime::spawn_targets as begin;\n',
                          'use crate::adapters::tui::new_helper as begin;\n'):
            with self.subTest(injection=injection):
                self.rejected(checker.PATHS[0], injection + LOGIN.replace('open() }', 'begin() }', 1), 'login.rs')

    def test_new_helper_without_alias(self):
        helper = self.root / 'src/adapters/tui/new_helper.rs'
        helper.parent.mkdir(parents=True, exist_ok=True)
        helper.write_text('pub fn begin() { tokio::spawn(async {}); }\n', encoding='utf-8')
        source = 'use crate::adapters::tui::new_helper;\n' + LOGIN.replace('open() }', 'new_helper::begin() }', 1)
        self.rejected(checker.PATHS[0], source, 'login.rs.*unapproved helper import')

    def test_unknown_callback(self):
        source = LOGIN.replace('open() }', 'args.map(crate::helper::begin) }', 1)
        self.rejected(checker.PATHS[0], source, 'login.rs.*callback')

    def test_unknown_method_receiver(self):
        source = LOGIN.replace('open() }', 'helper.clone() }', 1)
        self.rejected(checker.PATHS[0], source, 'login.rs.*receiver')

    def test_executable_in_handler_macro(self):
        source = LOGIN.replace('open() }', 'format!("{}", crate::helper::begin()) }', 1)
        self.rejected(checker.PATHS[0], source, 'login.rs.*format')

    def test_nested_validation_spawn(self):
        self.rejected(checker.PATHS[3], LOGIN_WORK + '\nasync fn hidden() { tokio::spawn(validate_target()); }', 'login_runtime.rs')

    def test_spawn_in_work_macro(self):
        self.rejected(checker.PATHS[3], LOGIN_WORK + '\nasync fn hidden() { vec![tokio::spawn(validate_target())]; }', 'login_runtime.rs')

    def test_aliased_runtime_spawn(self):
        self.rejected(checker.PATHS[4], 'use tokio::task::spawn as launch;\n' + ACCOUNT_WORK.replace('async fn run_change() {}', 'async fn run_change() { launch(work()); }'), 'accounts_runtime.rs.*launcher')

    def test_aliased_spawn_value(self):
        self.rejected(checker.PATHS[3], LOGIN_WORK + '\nasync fn hidden() { let launch = tokio::spawn; launch(work()); }', 'login_runtime.rs.*launcher')

    def test_executable_payload(self):
        for payload in ('impl Future<Output = ()>', 'Box<dyn Fn()>'):
            with self.subTest(payload=payload):
                self.rejected(checker.PATHS[2], PAYLOAD.replace('login_id: u64', 'login_id: ' + payload), 'AuthWork')

    def test_payload_type_shadowing(self):
        for source in (
            PAYLOAD.replace('login_id: u64', 'login_id: evil::LoginPlan'),
            PAYLOAD + '\ntype String = Box<dyn Fn()>;\n',
        ):
            with self.subTest(source=source):
                self.rejected(checker.PATHS[2], source, 'AuthWork')

    def test_unknown_macro_and_glob(self):
        for addition in ('use crate::domain::models::*;\n', 'fn surprise() { launch!(); }\n'):
            with self.subTest(addition=addition):
                self.rejected(checker.PATHS[0], LOGIN + addition, 'login.rs')

    def test_discarded_producer_despite_other_spawns(self):
        self.rejected(checker.PATHS[5], DISPATCH.replace('let outcome = handlers::accounts::command();', 'handlers::accounts::command();\n let outcome = HandlerOutcome::Quiet;'), 'accounts::command')

    def test_discarded_conditional_producer(self):
        changed = DISPATCH.replace('let outcome = handlers::accounts::command();',
                                   'let outcome = if flag { handlers::accounts::command(); HandlerOutcome::Quiet } else { HandlerOutcome::Quiet };')
        self.rejected(checker.PATHS[5], changed, 'accounts::command')

    def test_nested_handler_import_and_value_shadowing(self):
        for expression in (
            'use crate::adapters::tui::new_helper::begin as start; start()',
            'let start = crate::adapters::tui::new_helper::begin; start()',
            'fn start() -> HandlerOutcome { crate::adapters::tui::new_helper::begin() } start()',
        ):
            with self.subTest(expression=expression):
                self.rejected(checker.PATHS[0], LOGIN.replace('open() }', expression + ' }', 1),
                              r'login.rs.*(?:alias|import|shadow|binding|edge|callable|function)')

    def test_handler_callable_parameters_cannot_shadow_start(self):
        for parameter in ('start: fn() -> HandlerOutcome', 'start: &dyn Fn() -> HandlerOutcome'):
            with self.subTest(parameter=parameter):
                source = LOGIN.replace('fn command() -> HandlerOutcome { open() }',
                                       f'fn command({parameter}) -> HandlerOutcome {{ start() }}')
                self.rejected(checker.PATHS[0], source, r'login.rs.*(?:shadow|binding|callable)')

    def test_handler_macro_definition_cannot_shadow_format(self):
        definition = 'macro_rules! format { ($($t:tt)*) => { crate::adapters::tui::new_helper::begin() }; }'
        invocation = 'format!("Login failed: {}", display_text(error));'
        for addition in (
            definition + '\nfn failed_to_start() { ' + invocation + ' }',
            'fn failed_to_start() { ' + definition + ' ' + invocation + ' }',
        ):
            with self.subTest(addition=addition):
                self.rejected(checker.PATHS[0], LOGIN + addition, r'login.rs.*macro')

    def test_event_handler_intermediary_is_inspected(self):
        self.rejected(checker.PATHS[0],
                      LOGIN + '\nfn on_event() { crate::adapters::tui::new_helper::begin(); }',
                      r'login.rs.*(?:edge|callable)')

    def test_worker_generic_and_joinset_launches(self):
        for body in (
            'tokio::runtime::Handle::current().spawn::<_>(async {});',
            'tokio::runtime::Handle::current().spawn_blocking::<_, _>(|| {});',
            'let mut tasks = tokio::task::JoinSet::new(); tasks.spawn_blocking_on(|| {}, &tokio::runtime::Handle::current());',
        ):
            with self.subTest(body=body):
                self.rejected(checker.PATHS[3], LOGIN_WORK + '\nasync fn hidden() { ' + body + ' }',
                              r'login_runtime.rs.*task (?:spawn|launcher)')

    def test_worker_reexported_launchers_and_callbacks(self):
        for addition in (
            'use crate::infrastructure::launchers::begin;\nasync fn hidden() { begin(async {}); }',
            'async fn hidden() { let begin = crate::infrastructure::launchers::begin; begin(async {}); }',
            'const emit: fn() = crate::infrastructure::launchers::Begin; fn hidden() { emit(); }',
            'fn hidden(begin: fn()) { begin(); }',
            'fn hidden(begin: &dyn Fn()) { begin(); }',
        ):
            with self.subTest(addition=addition):
                self.rejected(checker.PATHS[3], LOGIN_WORK + '\n' + addition,
                              r'login_runtime.rs.*(?:import|edge|binding|callable|parameter)')

    def test_ungated_tests_module_is_production(self):
        self.rejected(checker.PATHS[3], LOGIN_WORK + '''
mod tests { pub fn begin() { tokio::spawn(async {}); } }
async fn hidden() { tests::begin(); }
''', r'login_runtime.rs.*(?:spawn|launcher)')

    def test_gated_tests_module_is_not_production(self):
        self.sources[checker.PATHS[3]] += '\n#[cfg(test)] mod tests { fn begin() { tokio::spawn(async {}); } }\n'
        self.put()
        checker.check(self.root)

    def test_outcome_cannot_be_consumed_rebound_or_assigned(self):
        declaration = 'let outcome = handlers::accounts::command();'
        for intervening in (
            'let _discarded = outcome; let outcome = HandlerOutcome::Quiet;',
            'let outcome = HandlerOutcome::Quiet;',
            'outcome = HandlerOutcome::Quiet;',
            'consume(outcome);',
        ):
            with self.subTest(intervening=intervening):
                self.rejected(checker.PATHS[5], DISPATCH.replace(declaration, declaration + intervening),
                              r'accounts::command')

    def test_work_binding_cannot_be_replaced(self):
        source = DISPATCH.replace('=> match work {',
                                  '=> { let work = other; match work {', 1)
        source = source.replace('}, }, _ => {} }', '}, } }, _ => {} }', 1)
        self.rejected(checker.PATHS[5], source, r'login::command')

    def test_worker_future_must_be_executed_not_just_nested_in_spawn(self):
        original = 'tokio::spawn(run_providers());'
        for replacement in (
            'tokio::spawn(async move { let _never_run = || run_providers(); });',
            'tokio::spawn(async move { let _never_polled = async { run_providers().await; }; });',
            'let _never_launched = || tokio::spawn(run_providers());',
        ):
            with self.subTest(replacement=replacement):
                self.rejected(checker.PATHS[5], DISPATCH.replace(original, replacement),
                              r'login::command')

    def test_awaited_worker_in_spawn_is_accepted(self):
        self.sources[checker.PATHS[5]] = DISPATCH.replace(
            'tokio::spawn(run_providers());', 'tokio::spawn(async move { run_providers().await; });')
        self.put()
        checker.check(self.root)

    def test_change_is_exactly_fieldless_logout_select(self):
        for change in ('enum Change { Logout { after: fn() }, Select }',
                       'enum Change { Logout(Box<dyn Fn()>), Select }'):
            with self.subTest(change=change):
                self.rejected(checker.PATHS[4],
                              ACCOUNT_WORK.replace('enum Change { Logout, Select }', change),
                              r'accounts_runtime.rs.*Change')

    def test_invalid_and_missing_source(self):
        self.rejected(checker.PATHS[0], 'fn command( {', 'parse')
        self.sources[checker.PATHS[0]] = LOGIN
        self.put()
        (self.root / checker.PATHS[1]).unlink()
        with self.assertRaisesRegex(checker.BoundaryError, 'unavailable'):
            checker.check(self.root)

    def test_reviewed_refresh_checkpoints_are_awaited_under_instrumentation_cfg(self):
        for point in ('BeforeCompletion', 'CompletionFinished'):
            with self.subTest(point=point):
                self.sources[checker.PATHS[3]] = LOGIN_WORK.replace(
                    'async fn run_login() {}', '''async fn run_login() {
    #[cfg(any(test, feature = "test-instrumentation"))]
    target
        .refresh_order
        .checkpoint(crate::adapters::model_catalog_cache::RefreshCheckpoint::''' + point + ''')
        .await;
}''')
                self.put()
                checker.check(self.root)

    def test_refresh_checkpoint_requires_exact_instrumentation_cfg(self):
        for gate in ('', '#[cfg(debug_assertions)]', '#[cfg(feature = "other")]'):
            with self.subTest(gate=gate):
                source = LOGIN_WORK.replace('async fn run_login() {}', '''async fn run_login() {
''' + gate + '''
    target.refresh_order.checkpoint(
        crate::adapters::model_catalog_cache::RefreshCheckpoint::BeforeCompletion).await;
}''')
                self.rejected(checker.PATHS[3], source, r'login_runtime.rs.*checkpoint')

    def test_refresh_checkpoint_lookalike_receivers_and_methods_are_rejected(self):
        for expression in (
            'target.checkpoint(POINT).await',
            'target.launcher.checkpoint(POINT).await',
            'ctx.refresh_order.checkpoint(POINT).await',
            'target.refresh_order.begin(POINT).await',
            'target.refresh_order.spawn(async {})',
        ):
            with self.subTest(expression=expression):
                source = LOGIN_WORK.replace('async fn run_login() {}', '''async fn run_login() {
    #[cfg(any(test, feature = "test-instrumentation"))]
    ''' + expression.replace('POINT', 'crate::adapters::model_catalog_cache::RefreshCheckpoint::BeforeCompletion') + ''';
}''')
                self.rejected(checker.PATHS[3], source,
                              r'login_runtime.rs.*(?:checkpoint|receiver|spawn|launcher)')

    def test_refresh_checkpoint_is_not_a_callable_or_unpolled_future(self):
        for expression in (
            'let callback = target.refresh_order.checkpoint',
            'let callback = target.refresh_order.checkpoint(POINT)',
            'target.refresh_order.checkpoint(POINT)',
            'tokio::spawn(target.refresh_order.checkpoint(POINT))',
            'let callback = target.refresh_order.checkpoint(POINT).await',
        ):
            with self.subTest(expression=expression):
                source = LOGIN_WORK.replace('async fn run_login() {}', '''async fn run_login() {
    #[cfg(any(test, feature = "test-instrumentation"))]
    ''' + expression.replace('POINT', 'crate::adapters::model_catalog_cache::RefreshCheckpoint::BeforeCompletion') + ''';
}''')
                self.rejected(checker.PATHS[3], source,
                              r'login_runtime.rs.*(?:checkpoint|spawn|launcher)')

    def test_refresh_checkpoint_arguments_are_reviewed_completion_tags_only(self):
        for arguments in (
            '',
            'point',
            'crate::adapters::model_catalog_cache::RefreshCheckpoint::BeforePublication',
            'crate::adapters::model_catalog_cache::RefreshCheckpoint::BeforeCompletion, callback',
        ):
            with self.subTest(arguments=arguments):
                source = LOGIN_WORK.replace('async fn run_login() {}', '''async fn run_login() {
    #[cfg(any(test, feature = "test-instrumentation"))]
    target.refresh_order.checkpoint(''' + arguments + ''').await;
}''')
                self.rejected(checker.PATHS[3], source, r'login_runtime.rs.*checkpoint')

    def test_refresh_checkpoint_is_not_admitted_in_other_workers(self):
        for path, baseline, worker in (
            (checker.PATHS[3], LOGIN_WORK, 'prepare_targets'),
            (checker.PATHS[4], ACCOUNT_WORK, 'run_change'),
        ):
            with self.subTest(path=path, worker=worker):
                source = baseline.replace('async fn ' + worker + '() {}', '''async fn ''' + worker + '''() {
    #[cfg(any(test, feature = "test-instrumentation"))]
    target.refresh_order.checkpoint(
        crate::adapters::model_catalog_cache::RefreshCheckpoint::BeforeCompletion).await;
}''')
                self.rejected(path, source, r'runtime.rs.*checkpoint')


    def test_reviewed_completion_callback_is_awaited_inline(self):
        self.sources[checker.PATHS[3]] = LOGIN_WORK.replace(
            'async fn run_login() {}',
            'async fn run_login() { report.with_current(&target, || { ctx.emit(event); }).await; }')
        self.put()
        checker.check(self.root)

    def test_completion_callback_does_not_admit_other_receivers_or_callables(self):
        for expression in (
            'target.with_current(&target, || {}).await',
            'report.with_current(&other, || {}).await',
            'report.with_current(&target, callback).await',
            'report.with_current(&target, || {})',
            'let callback = report.with_current',
            'report.with_current(&target, || { target.refresh_order.spawn(async {}); }).await',
        ):
            with self.subTest(expression=expression):
                source = LOGIN_WORK.replace('async fn run_login() {}',
                                           'async fn run_login() { ' + expression + '; }')
                self.rejected(checker.PATHS[3], source,
                              r'login_runtime.rs.*(?:completion|receiver|spawn|launcher)')


    def test_completion_waits_cannot_escape_as_macro_callable_values(self):
        for expression in ('target.refresh_order.checkpoint', 'report.with_current'):
            with self.subTest(expression=expression):
                source = LOGIN_WORK.replace('async fn run_login() {}',
                                           'async fn run_login() { vec![' + expression + ']; }')
                self.rejected(checker.PATHS[3], source, r'login_runtime.rs.*completion')


if __name__ == '__main__':
    unittest.main()
