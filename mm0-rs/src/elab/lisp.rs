pub mod parser;
pub mod eval;

use std::ops::Deref;
use std::hash::Hash;
use std::sync::{Arc, Mutex};
use std::collections::HashMap;
use num::BigInt;
use crate::parser::ast::{Atom};
use crate::util::{ArcString, Span};
use super::{AtomID, AtomVec, Remap};
use parser::{IR, Branch};

pub enum Syntax {
  Define,
  Lambda,
  Quote,
  Unquote,
  If,
  Focus,
  Let,
  Letrec,
  Match,
  MatchFn,
  MatchFns,
}

impl Syntax {
  pub fn from_str(s: &str) -> Option<Syntax> {
    match s {
      "def" => Some(Syntax::Define),
      "fn" => Some(Syntax::Lambda),
      "quote" => Some(Syntax::Quote),
      "unquote" => Some(Syntax::Unquote),
      "if" => Some(Syntax::If),
      "focus" => Some(Syntax::Focus),
      "let" => Some(Syntax::Let),
      "letrec" => Some(Syntax::Letrec),
      "match" => Some(Syntax::Match),
      "match-fn" => Some(Syntax::MatchFn),
      "match-fn*" => Some(Syntax::MatchFns),
      _ => None
    }
  }

  pub fn parse(s: &str, a: Atom) -> Result<Syntax, &str> {
    match a {
      Atom::Ident => Self::from_str(s).ok_or(s),
      Atom::Quote => Ok(Syntax::Quote),
      Atom::Unquote => Ok(Syntax::Unquote),
      Atom::Nfx => Err(":nfx"),
    }
  }
}

pub type LispVal = Arc<LispKind>;
pub enum LispKind {
  Atom(AtomID),
  List(Vec<LispVal>),
  DottedList(Vec<LispVal>, LispVal),
  Number(BigInt),
  String(String),
  UnparsedFormula(String),
  Bool(bool),
  Syntax(Syntax),
  Undef,
  Proc(Proc),
  AtomMap(HashMap<AtomID, LispVal>),
  Ref(Mutex<LispVal>),
  MVar(usize, ArcString, bool),
  Goal(LispVal),
}
lazy_static! {
  pub static ref UNDEF: LispVal = Arc::new(LispKind::Undef);
  pub static ref TRUE: LispVal = Arc::new(LispKind::Bool(true));
  pub static ref FALSE: LispVal = Arc::new(LispKind::Bool(false));
  pub static ref NIL: LispVal = Arc::new(LispKind::List(vec![]));
}

pub enum Proc {
  Builtin(BuiltinProc),
  LambdaExact(Span, Vec<LispVal>, usize, Arc<IR>),
  LambdaAtLeast(Span, Vec<LispVal>, usize, Arc<IR>),
  MatchCont(Span, Vec<LispVal>, LispVal, Arc<[Branch]>, usize),
}

#[derive(Copy, Clone)]
pub enum BuiltinProc {
  NewRef,
  SetRef,
}

#[derive(Default)]
pub struct LispRemapper {
  pub atom: AtomVec<AtomID>,
  pub lisp: HashMap<*const LispKind, LispVal>,
}
impl Remap<LispRemapper> for AtomID {
  fn remap(&self, r: &mut LispRemapper) -> Self { *r.atom.get(*self).unwrap_or(self) }
}
impl<R, K: Clone + Hash + Eq, V: Remap<R>> Remap<R> for HashMap<K, V> {
  fn remap(&self, r: &mut R) -> Self { self.iter().map(|(k, v)| (k.clone(), v.remap(r))).collect() }
}
impl<R, A: Remap<R>> Remap<R> for Mutex<A> {
  fn remap(&self, r: &mut R) -> Self { Mutex::new(self.lock().unwrap().remap(r)) }
}
impl Remap<LispRemapper> for LispVal {
  fn remap(&self, r: &mut LispRemapper) -> Self {
    let p: *const LispKind = self.deref();
    if let Some(v) = r.lisp.get(&p) {return v.clone()}
    let v = match self.deref() {
      LispKind::Atom(a) => Arc::new(LispKind::Atom(a.remap(r))),
      LispKind::List(v) => Arc::new(LispKind::List(v.remap(r))),
      LispKind::DottedList(v, l) => Arc::new(LispKind::DottedList(v.remap(r), l.remap(r))),
      LispKind::Proc(f) => Arc::new(LispKind::Proc(f.remap(r))),
      LispKind::AtomMap(m) => Arc::new(LispKind::AtomMap(m.remap(r))),
      LispKind::Ref(m) => Arc::new(LispKind::Ref(m.remap(r))),
      _ => self.clone(),
    };
    r.lisp.entry(p).or_insert(v).clone()
  }
}
impl Remap<LispRemapper> for Proc {
  fn remap(&self, r: &mut LispRemapper) -> Self {
    match self {
      &Proc::Builtin(p) => Proc::Builtin(p),
      &Proc::LambdaExact(sp, ref env, n, ref c) =>
        Proc::LambdaExact(sp, env.remap(r), n, c.remap(r)),
      &Proc::LambdaAtLeast(sp, ref env, n, ref c) =>
        Proc::LambdaAtLeast(sp, env.remap(r), n, c.remap(r)),
      &Proc::MatchCont(sp, ref env, ref e, ref brs, i) =>
        Proc::MatchCont(sp, env.remap(r), e.remap(r), brs.remap(r), i),
    }
  }
}