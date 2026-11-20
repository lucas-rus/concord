//! # concord-lang
//!
//! Language front-ends that turn source files into Concord symbols (with
//! contract and body digests), import bindings and resolved references.
//! Python is supported first; TypeScript is planned.

pub mod python;

use concord_core::{CallArgs, Symbol, SymbolId, SymbolKind, UseSite};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};

#[derive(Debug, thiserror::Error)]
pub enum LangError {
    #[error("cannot load grammar: {0}")]
    Grammar(String),
    #[error("cannot parse {0}")]
    Parse(String),
}

/// `local` name bound in a file to a dotted `target` (module or module attribute).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportBinding {
    pub local: String,
    pub target: String,
    pub line: u32,
}

/// An unresolved reference: a name or attribute chain used inside `from`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawRef {
    pub from: SymbolId,
    pub chain: Vec<String>,
    pub line: u32,
    pub text: String,
    pub is_call: bool,
    /// Argument shape for calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<CallArgs>,
}

/// Everything Concord knows about one source file.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileIndex {
    pub path: String,
    pub module: String,
    pub symbols: Vec<Symbol>,
    pub imports: Vec<ImportBinding>,
    pub refs: Vec<RawRef>,
    /// The parser recovered from syntax errors (results may be partial).
    pub has_errors: bool,
}

/// Index a file if its language is supported.
pub fn index_file(path: &str, source: &str) -> Option<Result<FileIndex, LangError>> {
    if python::is_python_path(path) {
        Some(python::index_file(path, source))
    } else {
        None
    }
}

/// A repository-wide index used to resolve references to symbols.
#[derive(Clone, Debug, Default)]
pub struct RepoIndex {
    files: BTreeMap<String, FileIndex>,
    by_id: HashMap<SymbolId, (String, usize)>,
    callables_by_name: HashMap<String, Vec<SymbolId>>,
}

impl RepoIndex {
    pub fn new(files: impl IntoIterator<Item = FileIndex>) -> Self {
        let mut idx = RepoIndex::default();
        for f in files {
            idx.files.insert(f.path.clone(), f);
        }
        idx.rebuild();
        idx
    }

    /// Replace (or add) a file and rebuild lookup tables.
    pub fn upsert(&mut self, file: FileIndex) {
        self.files.insert(file.path.clone(), file);
        self.rebuild();
    }

    pub fn remove(&mut self, path: &str) {
        self.files.remove(path);
        self.rebuild();
    }

    fn rebuild(&mut self) {
        self.by_id.clear();
        self.callables_by_name.clear();
        for (path, f) in &self.files {
            for (i, s) in f.symbols.iter().enumerate() {
                self.by_id.insert(s.id.clone(), (path.clone(), i));
                if matches!(s.kind, SymbolKind::Method | SymbolKind::Function) {
                    self.callables_by_name
                        .entry(s.id.name().to_owned())
                        .or_default()
                        .push(s.id.clone());
                }
            }
        }
    }

    pub fn files(&self) -> impl Iterator<Item = &FileIndex> {
        self.files.values()
    }

    pub fn file(&self, path: &str) -> Option<&FileIndex> {
        self.files.get(path)
    }

    pub fn symbol(&self, id: &SymbolId) -> Option<&Symbol> {
        self.by_id.get(id).map(|(p, i)| &self.files[p].symbols[*i])
    }

    pub fn symbols(&self) -> impl Iterator<Item = &Symbol> {
        self.files.values().flat_map(|f| f.symbols.iter())
    }

    /// Symbols whose definitions overlap the given 1-based line range of a file
    /// (used to map `Read(file, offset, limit)` tool calls to symbol reads).
    pub fn symbols_in_lines(&self, path: &str, start: u32, end: u32) -> Vec<SymbolId> {
        self.files
            .get(path)
            .map(|f| {
                f.symbols
                    .iter()
                    .filter(|s| s.span.overlaps(start, end))
                    .map(|s| s.id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Resolve a raw reference made in `file` to a known symbol.
    ///
    /// Strategy (sound-ish over-approximation, see docs/design.md):
    /// 1. `self.x` / `cls.x` inside a method -> member of the enclosing class;
    /// 2. names bound by imports (incl. `import a.b` chains and `*` imports);
    /// 3. module-level names of the same module;
    /// 4. for attribute *calls* on unknown receivers, a method/function with
    ///    that name if it is unique in the repository.
    pub fn resolve(&self, file: &FileIndex, r: &RawRef) -> Option<SymbolId> {
        let head = r.chain.first()?;
        let rest = &r.chain[1..];

        if (head == "self" || head == "cls")
            && !rest.is_empty()
            && let Some(class) = self.enclosing_class(&r.from)
        {
            // `self.x.y`: only a known member counts. Reading an instance attribute
            // is not a use of the class (its constructor contract).
            return self.longest_prefix(class.as_str(), rest).filter(|s| s != &class);
        }

        // Later bindings shadow earlier ones.
        if let Some(b) = file.imports.iter().rev().find(|b| &b.local == head)
            && let Some(s) = self.longest_prefix(&b.target, rest)
        {
            return Some(s);
        }

        if let Some(s) = self.longest_prefix(&format!("{}.{}", file.module, head), rest)
            && !file.module.is_empty()
        {
            return Some(s);
        }
        if file.module.is_empty()
            && let Some(s) = self.longest_prefix(head, rest)
        {
            return Some(s);
        }

        for b in file.imports.iter().filter(|b| b.local == "*") {
            if let Some(s) = self.longest_prefix(&format!("{}.{}", b.target, head), rest) {
                return Some(s);
            }
        }

        if r.is_call && !rest.is_empty() {
            let name = r.chain.last()?;
            if let Some(c) = self.callables_by_name.get(name)
                && c.len() == 1
            {
                return Some(c[0].clone());
            }
        }
        None
    }

    /// Resolved uses made from the given symbols (and, optionally, from the
    /// module-level code of the given files).
    pub fn uses_from(
        &self,
        from: &BTreeSet<SymbolId>,
        module_level_files: &BTreeSet<String>,
    ) -> BTreeMap<SymbolId, Vec<UseSite>> {
        let mut out: BTreeMap<SymbolId, Vec<UseSite>> = BTreeMap::new();
        for f in self.files.values() {
            let module_level = module_level_files.contains(&f.path);
            for r in &f.refs {
                let is_module_ref = r.from.as_str() == f.module;
                if !(from.contains(&r.from) || (module_level && is_module_ref)) {
                    continue;
                }
                let Some(target) = self.resolve(f, r) else { continue };
                if target == r.from {
                    continue;
                }
                let site = UseSite {
                    path: f.path.clone(),
                    line: r.line,
                    from: r.from.clone(),
                    text: r.text.clone(),
                    call: r.args.clone(),
                };
                let v = out.entry(target).or_default();
                if !v.contains(&site) {
                    v.push(site);
                }
            }
        }
        for v in out.values_mut() {
            v.sort();
        }
        out
    }

    fn enclosing_class(&self, from: &SymbolId) -> Option<SymbolId> {
        let mut cur = from.parent().map(SymbolId::new);
        while let Some(c) = cur {
            if self.symbol(&c).is_some_and(|s| s.kind == SymbolKind::Class) {
                return Some(c);
            }
            cur = c.parent().map(SymbolId::new);
        }
        None
    }

    /// Longest known symbol among `base`, `base.rest[0]`, `base.rest[0].rest[1]`, ...
    fn longest_prefix(&self, base: &str, rest: &[String]) -> Option<SymbolId> {
        let mut candidates = vec![base.to_owned()];
        let mut cur = base.to_owned();
        for part in rest {
            cur.push('.');
            cur.push_str(part);
            candidates.push(cur.clone());
        }
        candidates
            .into_iter()
            .rev()
            .map(SymbolId::new)
            .find(|c| self.by_id.contains_key(c))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idx(files: &[(&str, &str)]) -> RepoIndex {
        RepoIndex::new(files.iter().map(|(p, s)| python::index_file(p, s).unwrap()))
    }

    #[test]
    fn resolves_cross_file_calls() {
        let r = idx(&[
            ("notify/__init__.py", ""),
            ("notify/email.py", "def send_email(to, body):\n    pass\n"),
            (
                "auth/reset.py",
                "from notify.email import send_email\n\ndef request_reset(user):\n    send_email(user.email, 'x')\n",
            ),
            (
                "app.py",
                "import notify.email\n\ndef main():\n    notify.email.send_email('a', 'b')\n",
            ),
        ]);
        let from: BTreeSet<SymbolId> = ["auth.reset.request_reset".into(), "app.main".into()].into();
        let uses = r.uses_from(&from, &BTreeSet::new());
        let sites = &uses[&SymbolId::new("notify.email.send_email")];
        assert_eq!(sites.len(), 2);
        assert!(sites.iter().any(|s| s.path == "auth/reset.py" && s.line == 4));
        assert!(sites.iter().any(|s| s.path == "app.py"));
    }

    #[test]
    fn resolves_self_methods_and_unique_method_names() {
        let r = idx(&[
            (
                "shop/cart.py",
                "class Cart:\n    def add(self, x):\n        self.check(x)\n    def check(self, x):\n        pass\n",
            ),
            ("shop/api.py", "def handler(cart):\n    cart.add(1)\n"),
        ]);
        let from: BTreeSet<SymbolId> = ["shop.cart.Cart.add".into(), "shop.api.handler".into()].into();
        let uses = r.uses_from(&from, &BTreeSet::new());
        assert!(uses.contains_key(&SymbolId::new("shop.cart.Cart.check")));
        assert!(uses.contains_key(&SymbolId::new("shop.cart.Cart.add")));
    }

    #[test]
    fn instance_attributes_are_not_uses_of_the_class() {
        let r = idx(&[(
            "m.py",
            "class L:\n    def __init__(self, p):\n        self.p = p\n    def f(self):\n        return self.p\n",
        )]);
        let uses = r.uses_from(&["m.L.f".into()].into(), &BTreeSet::new());
        assert!(!uses.contains_key(&SymbolId::new("m.L")), "{uses:?}");
    }

    #[test]
    fn module_level_imports_count_as_uses() {
        let r = idx(&[
            ("notify/email.py", "def send_email(to, body):\n    pass\n"),
            ("auth/reset.py", "from notify.email import send_email\n"),
        ]);
        let uses = r.uses_from(&BTreeSet::new(), &["auth/reset.py".to_string()].into());
        assert!(uses.contains_key(&SymbolId::new("notify.email.send_email")));
    }

    #[test]
    fn maps_line_ranges_to_symbols() {
        let r = idx(&[("m.py", "def a():\n    pass\n\ndef b():\n    pass\n")]);
        assert_eq!(r.symbols_in_lines("m.py", 4, 5), vec![SymbolId::new("m.b")]);
        assert_eq!(r.symbols_in_lines("m.py", 1, 5).len(), 2);
    }
}
