use super::environment_impl::{
    colon_split, uvars, EnvMutex, EnvMutexGuard, EnvScopedImpl, EnvStackImpl, ModResult,
    UVAR_SCOPE_IS_GLOBAL,
};
use crate::abbrs::{abbrs_get_set, Abbreviation, Position};
use crate::common::{unescape_string, UnescapeStringStyle};
use crate::env::{EnvMode, EnvStackSetResult, EnvVar, Statuses};
use crate::env_universal_common::{self, CallbackDataList, EnvUniversal, UniversalNotifier};
use crate::event::Event;
use crate::ffi;
use crate::flog::FLOG;
use crate::global_safety::RelaxedAtomicBool;
use crate::null_terminated_array::OwningNullTerminatedArray;
use crate::path::path_make_canonical;
use crate::wchar::{wstr, WExt, WString, L};
use crate::wchar_ffi::{AsWstr, WCharFromFFI, WCharToFFI};
use crate::wcstringutil::join_strings;
use crate::wutil::{wgetcwd, wgettext};

use autocxx::WithinUniquePtr;
use cxx::UniquePtr;
use lazy_static::lazy_static;
use libc::c_int;
use std::sync::{Arc, Mutex};

/// TODO: migrate to history once ported.
const DFLT_FISH_HISTORY_SESSION_ID: &wstr = L!("fish");

// Universal variables instance.
lazy_static! {
    static ref UVARS: Mutex<EnvUniversal> = Mutex::new(EnvUniversal::new());
}

/// Set when a universal variable has been modified but not yet been written to disk via sync().
static UVARS_LOCALLY_MODIFIED: RelaxedAtomicBool = RelaxedAtomicBool::new(false);

pub type EnvironmentRef = Arc<dyn Environment>;

/// An environment is read-only access to variable values.
pub trait Environment {
    /// Get a variable by name using default flags.
    fn get(&self, name: &wstr) -> Option<EnvVar> {
        self.getf(name, EnvMode::DEFAULT)
    }

    /// Get a variable by name using the specified flags.
    fn getf(&self, name: &wstr, mode: EnvMode) -> Option<EnvVar>;

    /// Return the list of variable names.
    fn get_names(&self, flags: EnvMode) -> Vec<WString>;

    /// Returns PWD with a terminating slash.
    fn get_pwd_slash(&self) -> WString {
        // Return "/" if PWD is missing.
        // See https://github.com/fish-shell/fish-shell/issues/5080
        let Some(var) = self.get(L!("PWD")) else {
            return WString::from("/");
        };
        let mut pwd = WString::new();
        if var.is_empty() {
            pwd = var.as_string();
        }
        if !pwd.ends_with('/') {
            pwd.push('/');
        }
        pwd
    }

    /// Get a variable by name using default flags, unless it is empty.
    fn get_unless_empty(&self, name: &wstr) -> Option<EnvVar> {
        self.getf_unless_empty(name, EnvMode::DEFAULT)
    }

    /// Get a variable by name using the given flags, unless it is empty.
    fn getf_unless_empty(&self, name: &wstr, mode: EnvMode) -> Option<EnvVar> {
        let var = self.getf(name, mode)?;
        if !var.is_empty() {
            return Some(var);
        }
        None
    }
}

/// The null environment contains nothing.
pub struct EnvNull;

impl EnvNull {
    pub fn new() -> EnvNull {
        EnvNull
    }
}

impl Environment for EnvNull {
    fn getf(&self, _name: &wstr, _mode: EnvMode) -> Option<EnvVar> {
        None
    }

    fn get_names(&self, _flags: EnvMode) -> Vec<WString> {
        Vec::new()
    }
}

/// A helper type for wrapping a type-erased Environment.
pub struct EnvDyn {
    inner: Box<dyn Environment>,
}

impl EnvDyn {
    fn new(inner: Box<dyn Environment>) -> Self {
        Self { inner }
    }
}

impl Environment for EnvDyn {
    fn getf(&self, key: &wstr, mode: EnvMode) -> Option<EnvVar> {
        self.inner.getf(key, mode)
    }

    fn get_names(&self, flags: EnvMode) -> Vec<WString> {
        self.inner.get_names(flags)
    }

    fn get_pwd_slash(&self) -> WString {
        self.inner.get_pwd_slash()
    }
}

/// An immutable environment, used in snapshots.
pub struct EnvScoped {
    inner: EnvMutex<EnvScopedImpl>,
}

impl EnvScoped {
    fn from_impl(inner: EnvMutex<EnvScopedImpl>) -> EnvScoped {
        EnvScoped { inner }
    }

    fn lock(&self) -> EnvMutexGuard<EnvScopedImpl> {
        self.inner.lock()
    }
}

/// A mutable environment which allows scopes to be pushed and popped.
/// This backs the parser's "vars".
pub struct EnvStack {
    inner: EnvMutex<EnvStackImpl>,
}

impl EnvStack {
    pub fn new() -> EnvStack {
        EnvStack {
            inner: EnvStackImpl::new(),
        }
    }

    fn lock(&self) -> EnvMutexGuard<EnvStackImpl> {
        self.inner.lock()
    }

    /// \return whether we are the principal stack.
    pub fn is_principal(&self) -> bool {
        self as *const Self == Arc::as_ptr(&*PRINCIPAL_STACK)
    }

    /// Helpers to get and set the proc statuses.
    /// These correspond to $status and $pipestatus.
    pub fn get_last_statuses(&self) -> Statuses {
        self.lock().base.get_last_statuses().clone()
    }

    pub fn get_last_status(&self) -> c_int {
        self.lock().base.get_last_statuses().status
    }

    pub fn set_last_statuses(&self, statuses: Statuses) {
        self.lock().base.set_last_statuses(statuses);
    }

    /// Sets the variable with the specified name to the given values.
    pub fn set(&self, key: &wstr, mode: EnvMode, mut vals: Vec<WString>) -> EnvStackSetResult {
        // Historical behavior.
        if vals.len() == 1 && (key == "PWD" || key == "HOME") {
            path_make_canonical(vals.first_mut().unwrap());
        }

        // Hacky stuff around PATH and CDPATH: #3914.
        // Not MANPATH; see #4158.
        // Replace empties with dot. Note we ignore pathvar here.
        if key == "PATH" || key == "CDPATH" {
            // Split on colons.
            let mut munged_vals = colon_split(&vals);
            // Replace empties with dots.
            for val in munged_vals.iter_mut() {
                if val.is_empty() {
                    val.push('.');
                }
            }
            vals = munged_vals;
        }

        let ret: ModResult = self.lock().set(key, mode, vals);
        if ret.status == EnvStackSetResult::ENV_OK {
            // If we modified the global state, or we are principal, then dispatch changes.
            // Important to not hold the lock here.
            if ret.global_modified || self.is_principal() {
                ffi::env_dispatch_var_change_ffi(&key.to_ffi() /* , self */);
            }
        }
        // Mark if we modified a uvar.
        if ret.uvar_modified {
            UVARS_LOCALLY_MODIFIED.store(true);
        }
        ret.status
    }

    /// Sets the variable with the specified name to a single value.
    pub fn set_one(&self, key: &wstr, mode: EnvMode, val: WString) -> EnvStackSetResult {
        self.set(key, mode, vec![val])
    }

    /// Sets the variable with the specified name to no values.
    pub fn set_empty(&self, key: &wstr, mode: EnvMode) -> EnvStackSetResult {
        self.set(key, mode, Vec::new())
    }

    /// Update the PWD variable based on the result of getcwd.
    pub fn set_pwd_from_getcwd(&self) {
        let cwd = wgetcwd();
        if cwd.is_empty() {
            FLOG!(
                error,
                wgettext!(
                    "Could not determine current working directory. Is your locale set correctly?"
                )
            );
        }
        self.set_one(L!("PWD"), EnvMode::EXPORT | EnvMode::GLOBAL, cwd);
    }

    /// Remove environment variable.
    ///
    /// \param key The name of the variable to remove
    /// \param mode should be ENV_USER if this is a remove request from the user, 0 otherwise. If
    /// this is a user request, read-only variables can not be removed. The mode may also specify
    /// the scope of the variable that should be erased.
    ///
    /// \return the set result.
    pub fn remove(&self, key: &wstr, mode: EnvMode) -> EnvStackSetResult {
        let ret = self.lock().remove(key, mode);
        #[allow(clippy::collapsible_if)]
        if ret.status == EnvStackSetResult::ENV_OK {
            if ret.global_modified || self.is_principal() {
                // Important to not hold the lock here.
                ffi::env_dispatch_var_change_ffi(&key.to_ffi() /*,  self */);
            }
        }
        if ret.uvar_modified {
            UVARS_LOCALLY_MODIFIED.store(true);
        }
        ret.status
    }

    /// Push the variable stack. Used for implementing local variables for functions and for-loops.
    pub fn push(&self, new_scope: bool) {
        let mut imp = self.lock();
        if new_scope {
            imp.push_shadowing();
        } else {
            imp.push_nonshadowing();
        }
    }

    /// Pop the variable stack. Used for implementing local variables for functions and for-loops.
    pub fn pop(&self) {
        let popped = self.lock().pop();
        // Only dispatch variable changes if we are the principal environment.
        if self.is_principal() {
            // TODO: we would like to coalesce locale / curses changes, so that we only re-initialize
            // once.
            for key in popped {
                ffi::env_dispatch_var_change_ffi(&key.to_ffi() /*, self */);
            }
        }
    }

    /// Returns an array containing all exported variables in a format suitable for execv.
    pub fn export_array(&self) -> Arc<OwningNullTerminatedArray> {
        self.lock().base.export_array()
    }

    /// Snapshot this environment. This means returning a read-only copy. Local variables are copied
    /// but globals are shared (i.e. changes to global will be visible to this snapshot).
    pub fn snapshot(&self) -> EnvDyn {
        let scoped = EnvScoped::from_impl(self.lock().base.snapshot());
        EnvDyn {
            inner: Box::new(scoped) as Box<dyn Environment>,
        }
    }

    /// Synchronizes universal variable changes.
    /// If \p always is set, perform synchronization even if there's no pending changes from this
    /// instance (that is, look for changes from other fish instances).
    /// \return a list of events for changed variables.
    #[allow(clippy::vec_box)]
    pub fn universal_sync(&self, always: bool) -> Vec<Event> {
        if UVAR_SCOPE_IS_GLOBAL.load() {
            return Vec::new();
        }
        if !always && !UVARS_LOCALLY_MODIFIED.load() {
            return Vec::new();
        }
        UVARS_LOCALLY_MODIFIED.store(false);

        let mut callbacks = CallbackDataList::new();
        let changed = uvars().sync(&mut callbacks);
        if changed {
            env_universal_common::default_notifier().post_notification();
        }
        // React internally to changes to special variables like LANG, and populate on-variable events.
        let mut result = Vec::new();
        #[allow(unreachable_code)]
        for callback in callbacks {
            let name = callback.key;
            ffi::env_dispatch_var_change_ffi(&name.to_ffi() /* , self */);
            let evt = if callback.val.is_none() {
                Event::variable_erase(name)
            } else {
                Event::variable_set(name)
            };
            result.push(evt);
        }
        result
    }

    /// A variable stack that only represents globals.
    /// Do not push or pop from this.
    pub fn globals() -> &'static EnvStackRef {
        &GLOBALS
    }

    /// Access the principal variable stack, associated with the principal parser.
    pub fn principal() -> &'static EnvStackRef {
        &PRINCIPAL_STACK
    }

    pub fn set_argv(&self, argv: Vec<WString>) {
        self.set(L!("argv"), EnvMode::LOCAL, argv);
    }
}

impl Environment for EnvScoped {
    fn getf(&self, key: &wstr, mode: EnvMode) -> Option<EnvVar> {
        self.lock().getf(key, mode)
    }

    fn get_names(&self, flags: EnvMode) -> Vec<WString> {
        self.lock().get_names(flags)
    }

    fn get_pwd_slash(&self) -> WString {
        self.lock().get_pwd_slash()
    }
}

/// Necessary for Arc<EnvStack> to be sync.
/// Safety: again, the global lock.
unsafe impl Send for EnvStack {}

impl Environment for EnvStack {
    fn getf(&self, key: &wstr, mode: EnvMode) -> Option<EnvVar> {
        self.lock().getf(key, mode)
    }

    fn get_names(&self, flags: EnvMode) -> Vec<WString> {
        self.lock().get_names(flags)
    }

    fn get_pwd_slash(&self) -> WString {
        self.lock().get_pwd_slash()
    }
}

pub type EnvStackRef = Arc<EnvStack>;

// A variable stack that only represents globals.
// Do not push or pop from this.
lazy_static! {
    static ref GLOBALS: EnvStackRef = Arc::new(EnvStack::new());
}

// Our singleton "principal" stack.
lazy_static! {
    static ref PRINCIPAL_STACK: EnvStackRef = Arc::new(EnvStack::new());
}

// Note: this is an incomplete port of env_init(); the rest remains in C++.
pub fn env_init(do_uvars: bool) {
    if !do_uvars {
        UVAR_SCOPE_IS_GLOBAL.store(true);
    } else {
        // let vars = EnvStack::principal();

        // Set up universal variables using the default path.
        let mut callbacks = CallbackDataList::new();
        uvars().initialize(&mut callbacks);
        for callback in callbacks {
            ffi::env_dispatch_var_change_ffi(&callback.key.to_ffi() /* , vars */);
        }

        // Do not import variables that have the same name and value as
        // an exported universal variable. See issues #5258 and #5348.
        let uvars_locked = uvars();
        let table = uvars_locked.get_table();
        for (name, uvar) in table {
            if !uvar.exports() {
                continue;
            }

            // Look for a global exported variable with the same name.
            let global = EnvStack::globals().getf(name, EnvMode::GLOBAL | EnvMode::EXPORT);
            if global.is_some() && global.unwrap().as_string() == uvar.as_string() {
                EnvStack::globals().remove(name, EnvMode::GLOBAL | EnvMode::EXPORT);
            }
        }

        // Import any abbreviations from uvars.
        // Note we do not dynamically react to changes.
        let prefix = L!("_fish_abbr_");
        let prefix_len = prefix.char_count();
        let from_universal = true;
        let mut abbrs = abbrs_get_set();
        for (name, uvar) in table {
            if !name.starts_with(prefix) {
                continue;
            }
            let escaped_name = name.slice_from(prefix_len);
            if let Some(name) = unescape_string(escaped_name, UnescapeStringStyle::Var) {
                let key = name.clone();
                let replacement: WString = join_strings(uvar.as_list(), ' ');
                abbrs.add(Abbreviation::new(
                    name,
                    key,
                    replacement,
                    Position::Command,
                    from_universal,
                ));
            }
        }
    }
}
/// A test environment that knows about PWD.
// TODO Post-FFI: this should be cfg(test).
pub mod test {
    use crate::env::{EnvMode, EnvVar, EnvVarFlags, Environment};
    use crate::wchar::{wstr, WString};
    use crate::wutil::wgetcwd;
    use std::collections::HashMap;
    use widestring_suffix::widestrs;

    /// An environment built around an std::map.
    #[derive(Default)]
    pub struct TestEnvironment {
        pub vars: HashMap<WString, WString>,
    }
    impl Environment for TestEnvironment {
        fn getf(&self, name: &wstr, mode: EnvMode) -> Option<EnvVar> {
            self.vars
                .get(name)
                .map(|value| EnvVar::new(value.clone(), EnvVarFlags::default()))
        }
        fn get_names(&self, flags: EnvMode) -> Vec<WString> {
            self.vars.keys().cloned().collect()
        }
    }
    #[derive(Default)]
    pub struct PwdEnvironment {
        pub parent: TestEnvironment,
    }
    #[widestrs]
    impl Environment for PwdEnvironment {
        fn getf(&self, name: &wstr, mode: EnvMode) -> Option<EnvVar> {
            if name == "PWD"L {
                return Some(EnvVar::new(wgetcwd(), EnvVarFlags::default()));
            }
            self.parent.getf(name, mode)
        }

        fn get_names(&self, flags: EnvMode) -> Vec<WString> {
            let mut res = self.parent.get_names(flags);
            if !res.iter().any(|n| n == "PWD"L) {
                res.push("PWD"L.to_owned());
            }
            res
        }
    }
}
