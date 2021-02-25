//! MMB exporter, which produces `.mmb` binary proof files from an
//! [`Environment`](crate::elab::Environment) object.
use std::convert::TryInto;
use std::mem;
use std::io::{self, Write, Seek, SeekFrom};
use byteorder::{LE, ByteOrder, WriteBytesExt};
use zerocopy::{AsBytes, LayoutVerified, U32, U64};
use crate::{
  Type, Expr, Proof, SortId, TermId, ThmId, AtomId, TermKind, ThmKind,
  TermVec, ExprNode, ProofNode, StmtTrace, DeclKey, Modifiers,
  FrozenEnv, FileRef, LinedString};

#[allow(clippy::wildcard_imports)]
use mmb_parser::{ProofCmd, UnifyCmd, cmd::*, write_cmd_bytes};

#[derive(Debug)]
struct Reorder<T=u32> {
  map: Box<[Option<T>]>,
  idx: u32,
}

impl<T> Reorder<T> {
  fn new(nargs: u32, len: usize, mut f: impl FnMut(u32) -> T) -> Reorder<T> {
    assert!(nargs as usize <= len);
    let mut map = Vec::with_capacity(len);
    map.extend((0..nargs).map(|i| Some(f(i))));
    map.resize_with(len, Default::default);
    let mut map: Box<[Option<T>]> = map.into();
    for i in 0..nargs {map[i as usize] = Some(f(i))}
    Reorder {map, idx: nargs}
  }
}

struct IndexHeader<'a> {
  sorts: &'a mut [U64<LE>],
  terms: &'a mut [U64<LE>],
  thms: &'a mut [U64<LE>]
}

impl<'a> IndexHeader<'a> {
  fn sort(&mut self, i: SortId) -> &mut U64<LE> { &mut self.sorts[i.0 as usize] }
  fn term(&mut self, i: TermId) -> &mut U64<LE> { &mut self.terms[i.0 as usize] }
  fn thm(&mut self, i: ThmId) -> &mut U64<LE> { &mut self.thms[i.0 as usize] }
}

/// The main exporter structure. This keeps track of the underlying writer,
/// as well as tracking values that are written out of order.
#[derive(Debug)]
pub struct Exporter<'a, W: Write + Seek> {
  /// The name of the input file. This is only used in the debugging data.
  file: FileRef,
  /// The source text of the input file. This is only used in the debugging data.
  source: Option<&'a LinedString>,
  /// The input environment.
  env: &'a FrozenEnv,
  /// The underlying writer, which must support [`Seek`] because we write some parts
  /// of the file out of order. The [`BigBuffer`] wrapper can be used to equip a
  /// writer that doesn't support it with a [`Seek`] implementation.
  w: W,
  /// The current byte position of the writer.
  pos: u64,
  /// The calculated reorder maps for terms encountered so far (see [`Reorder`]).
  term_reord: TermVec<Option<Reorder>>,
  /// A list of "fixups", which are writes that have to occur in places other
  /// than the current writer location. We buffer these to avoid too many seeks
  /// of the underlying writer.
  fixups: Vec<(u64, Value)>,
}

/// A chunk of data that needs to be written out of order.
#[derive(Debug)]
enum Value {
  /// A (little endian) 32 bit value
  U32(U32<LE>),
  /// A (little endian) 64 bit value
  U64(U64<LE>),
  /// An arbitrary length byte slice. (We could store everything like this but
  /// the `U32` and `U64` cases are common and this avoids some allocation.)
  Box(Box<[u8]>),
}

/// A type for a 32 bit fixup, representing a promise to write 32 bits at the stored
/// location. It is generated by [`fixup32`](Exporter::fixup32) method,
/// and it is marked `#[must_use]` because it should be consumed by
/// [`commit`](Fixup32::commit), which requires fulfilling the promise.
#[must_use] struct Fixup32(u64);

/// A type for a 64 bit fixup, representing a promise to write 64 bits at the stored
/// location. It is generated by [`fixup64`](Exporter::fixup64) method,
/// and it is marked `#[must_use]` because it should be consumed by
/// [`commit`](Fixup64::commit), which requires fulfilling the promise.
#[must_use] struct Fixup64(u64);

/// A type for an arbitrary size fixup, representing a promise to write some number of bytes
/// bits at the given location. It is generated by
/// [`fixup_large`](Exporter::fixup_large) method,
/// and it is marked `#[must_use]` because it should be consumed by
/// [`commit`](FixupLarge::commit), which requires fulfilling the promise.
#[must_use] struct FixupLarge(u64, Box<[u8]>);

impl Fixup32 {
  /// Write `val` to this fixup, closing it.
  fn commit_val<W: Write + Seek>(self, e: &mut Exporter<'_, W>, val: u32) {
    e.fixups.push((self.0, Value::U32(U32::new(val))))
  }
  /// Write the current position of the exporter to this fixup, closing it.
  fn commit<W: Write + Seek>(self, e: &mut Exporter<'_, W>) {
    let val = e.pos.try_into().expect("position out of range");
    self.commit_val(e, val)
  }
}

impl Fixup64 {
  /// Write `val` to this fixup, closing it.
  fn commit_val<W: Write + Seek>(self, e: &mut Exporter<'_, W>, val: u64) {
    e.fixups.push((self.0, Value::U64(U64::new(val))))
  }
  /// Write the current position of the exporter to this fixup, closing it.
  fn commit<W: Write + Seek>(self, e: &mut Exporter<'_, W>) {
    let val = e.pos;
    self.commit_val(e, val)
  }
  /// Drop the value, which has the effect of writing 0 to the original fixup.
  #[inline] fn cancel(self) { drop(self) }
}

impl std::ops::Deref for FixupLarge {
  type Target = [u8];
  fn deref(&self) -> &[u8] { &self.1 }
}
impl std::ops::DerefMut for FixupLarge {
  fn deref_mut(&mut self) -> &mut [u8] { &mut self.1 }
}

impl FixupLarge {
  /// Assume that the construction of the fixup is complete, and write the stored value.
  fn commit<W: Write + Seek>(self, e: &mut Exporter<'_, W>) {
    e.fixups.push((self.0, Value::Box(self.1)))
  }
}

impl<W: Write + Seek> Write for Exporter<'_, W> {
  fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
    self.write_all(buf)?;
    Ok(buf.len())
  }
  fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
    self.pos += buf.len() as u64;
    self.w.write_all(buf)
  }
  fn flush(&mut self) -> io::Result<()> { self.w.flush() }
}

fn write_expr_proof(w: &mut impl Write,
  heap: &[ExprNode],
  reorder: &mut Reorder,
  node: &ExprNode,
  save: bool
) -> io::Result<u32> {
  Ok(match *node {
    ExprNode::Ref(i) => match reorder.map[i] {
      None => {
        let n = write_expr_proof(w, heap, reorder, &heap[i], true)?;
        reorder.map[i] = Some(n);
        n
      }
      Some(n) => {ProofCmd::Ref(n).write_to(w)?; n}
    }
    ExprNode::Dummy(_, s) => {
      ProofCmd::Dummy(s).write_to(w)?;
      (reorder.idx, reorder.idx += 1).0
    }
    ExprNode::App(tid, ref es) => {
      for e in &**es {write_expr_proof(w, heap, reorder, e, false)?;}
      ProofCmd::Term {tid, save}.write_to(w)?;
      if save {(reorder.idx, reorder.idx += 1).0} else {0}
    }
  })
}

/// A wrapper around a writer that implements [`Write`]` + `[`Seek`] by internally buffering
/// all writes, writing to the underlying writer only once on [`Drop`].
#[derive(Debug)]
pub struct BigBuffer<W: Write> {
  buffer: io::Cursor<Vec<u8>>,
  w: W,
}

impl<W: Write> BigBuffer<W> {
  /// Creates a new buffer given an underlying writer.
  pub fn new(w: W) -> Self { Self {buffer: Default::default(), w} }
  /// Flushes the buffer to the underlying writer, consuming the result.
  /// (The [`Drop`] implementation will also do this, but this allows us
  /// to catch IO errors.)
  pub fn finish(mut self) -> io::Result<()> {
    self.w.write_all(&mem::take(self.buffer.get_mut()))
  }
}

impl<W: Write> Write for BigBuffer<W> {
  fn write(&mut self, buf: &[u8]) -> io::Result<usize> { self.buffer.write(buf) }
  fn flush(&mut self) -> io::Result<()> { self.buffer.flush() }
}

impl<W: Write> Seek for BigBuffer<W> {
  fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> { self.buffer.seek(pos) }
}

impl<W: Write> Drop for BigBuffer<W> {
  fn drop(&mut self) {
    self.w.write_all(self.buffer.get_ref()).expect("write failed in Drop impl")
  }
}

impl<'a, W: Write + Seek> Exporter<'a, W> {
  /// Construct a new [`Exporter`] from an input file `file` with text `source`,
  /// a source environment containing proved theorems, and output writer `w`.
  pub fn new(file: FileRef, source: Option<&'a LinedString>, env: &'a FrozenEnv, w: W) -> Self {
    Self {
      term_reord: TermVec(Vec::with_capacity(env.terms().len())),
      file, source, env, w, pos: 0, fixups: vec![]
    }
  }

  fn write_u32(&mut self, n: u32) -> io::Result<()> {
    WriteBytesExt::write_u32::<LE>(self, n)
  }

  fn write_u64(&mut self, n: u64) -> io::Result<()> {
    WriteBytesExt::write_u64::<LE>(self, n)
  }

  fn fixup32(&mut self) -> io::Result<Fixup32> {
    let f = Fixup32(self.pos);
    self.write_u32(0)?;
    Ok(f)
  }

  fn fixup64(&mut self) -> io::Result<Fixup64> {
    let f = Fixup64(self.pos);
    self.write_u64(0)?;
    Ok(f)
  }

  fn fixup_large(&mut self, size: usize) -> io::Result<FixupLarge> {
    let f = FixupLarge(self.pos, vec![0; size].into());
    self.write_all(&f)?;
    Ok(f)
  }

  #[inline]
  fn align_to(&mut self, n: u8) -> io::Result<u64> {
    #[allow(clippy::cast_possible_truncation)] // actual truncation
    let i = n.wrapping_sub(self.pos as u8) & (n - 1);
    self.write_all(&vec![0; i.into()])?;
    Ok(self.pos)
  }

  #[inline]
  fn write_sort_deps(&mut self, bound: bool, sort: SortId, deps: u64) -> io::Result<()> {
    self.write_u64(u64::from(bound) << 63 | u64::from(sort.0) << 56 | deps)
  }

  #[inline]
  fn write_term_header(header: &mut [u8], nargs: u16, sort: SortId, has_def: bool, p_term: u32) {
    LE::write_u16(&mut header[0..], nargs);
    header[2] = sort.0 | if has_def {0x80} else {0};
    LE::write_u32(&mut header[4..], p_term);
  }

  fn write_binders<T>(&mut self, args: &[(T, Type)]) -> io::Result<()> {
    let mut bv = 1;
    for (_, ty) in args {
      match *ty {
        Type::Bound(s) => {
          if bv >= (1 << 55) {panic!("more than 55 bound variables")}
          self.write_sort_deps(true, s, bv)?;
          bv *= 2;
        }
        Type::Reg(s, deps) => self.write_sort_deps(false, s, deps)?,
      }
    }
    Ok(())
  }

  fn write_expr_unify(&mut self,
    heap: &[ExprNode],
    reorder: &mut Reorder,
    node: &ExprNode,
    save: &mut Vec<usize>
  ) -> io::Result<()> {
    macro_rules! commit {($n:expr) => {
      for i in save.drain(..) {reorder.map[i] = Some($n)}
    }}
    match *node {
      ExprNode::Ref(i) => match reorder.map[i] {
        None => {
          save.push(i);
          self.write_expr_unify(heap, reorder, &heap[i], save)?
        }
        Some(n) => {
          UnifyCmd::Ref(n).write_to(self)?;
          commit!(n)
        }
      }
      ExprNode::Dummy(_, s) => {
        commit!(reorder.idx); reorder.idx += 1;
        UnifyCmd::Dummy(s).write_to(self)?
      }
      ExprNode::App(tid, ref es) => {
        if save.is_empty() {
          UnifyCmd::Term {tid, save: false}.write_to(self)?
        } else {
          commit!(reorder.idx); reorder.idx += 1;
          UnifyCmd::Term {tid, save: true}.write_to(self)?
        }
        for e in &**es {
          self.write_expr_unify(heap, reorder, e, save)?
        }
      }
    }
    Ok(())
  }

  fn write_proof(&self, w: &mut impl Write,
    heap: &[ProofNode],
    reorder: &mut Reorder,
    hyps: &[u32],
    node: &ProofNode,
    save: bool
  ) -> io::Result<u32> {
    Ok(match node {
      &ProofNode::Ref(i) => match reorder.map[i] {
        None => {
          let n = self.write_proof(w, heap, reorder, hyps, &heap[i], true)?;
          reorder.map[i] = Some(n);
          n
        }
        Some(n) => {ProofCmd::Ref(n).write_to(w)?; n}
      }
      &ProofNode::Dummy(_, s) => {
        ProofCmd::Dummy(s).write_to(w)?;
        (reorder.idx, reorder.idx += 1).0
      }
      &ProofNode::Term {term, ref args} => {
        for e in &**args {self.write_proof(w, heap, reorder, hyps, e, false)?;}
        ProofCmd::Term {tid: term, save}.write_to(w)?;
        if save {(reorder.idx, reorder.idx += 1).0} else {0}
      }
      &ProofNode::Hyp(n, _) => {
        ProofCmd::Ref(hyps[n]).write_to(w)?;
        hyps[n]
      }
      &ProofNode::Thm {thm, ref args, ref res} => {
        let (args, hs) = args.split_at(self.env.thm(thm).args.len());
        for e in hs {self.write_proof(w, heap, reorder, hyps, e, false)?;}
        for e in args {self.write_proof(w, heap, reorder, hyps, e, false)?;}
        self.write_proof(w, heap, reorder, hyps, res, false)?;
        ProofCmd::Thm {tid: thm, save}.write_to(w)?;
        if save {(reorder.idx, reorder.idx += 1).0} else {0}
      }
      ProofNode::Conv(p) => {
        let (e1, c, p) = &**p;
        self.write_proof(w, heap, reorder, hyps, e1, false)?;
        self.write_proof(w, heap, reorder, hyps, p, false)?;
        ProofCmd::Conv.write_to(w)?;
        self.write_conv(w, heap, reorder, hyps, c)?;
        if save {
          ProofCmd::Save.write_to(w)?;
          (reorder.idx, reorder.idx += 1).0
        } else {0}
      }
      ProofNode::Refl(_) |
      ProofNode::Sym(_) |
      ProofNode::Cong {..} |
      ProofNode::Unfold {..} => unreachable!(),
    })
  }

  fn write_conv(&self, w: &mut impl Write,
    heap: &[ProofNode],
    reorder: &mut Reorder,
    hyps: &[u32],
    node: &ProofNode,
  ) -> io::Result<()> {
    match node {
      &ProofNode::Ref(i) => match reorder.map[i] {
        None => {
          let e = &heap[i];
          match e {
            ProofNode::Refl(_) | ProofNode::Ref(_) =>
              self.write_conv(w, heap, reorder, hyps, e)?,
            _ => {
              ProofCmd::ConvCut.write_to(w)?;
              self.write_conv(w, heap, reorder, hyps, e)?;
              ProofCmd::ConvSave.write_to(w)?;
              reorder.map[i] = Some(reorder.idx);
              reorder.idx += 1;
            }
          };
        }
        Some(n) => ProofCmd::ConvRef(n).write_to(w)?,
      }
      ProofNode::Dummy(_, _) |
      ProofNode::Term {..} |
      ProofNode::Hyp(_, _) |
      ProofNode::Thm {..} |
      ProofNode::Conv(_) => unreachable!(),
      ProofNode::Refl(_) => ProofCmd::Refl.write_to(w)?,
      ProofNode::Sym(c) => {
        ProofCmd::Sym.write_to(w)?;
        self.write_conv(w, heap, reorder, hyps, c)?;
      }
      ProofNode::Cong {args, ..} => {
        ProofCmd::Cong.write_to(w)?;
        for a in &**args {self.write_conv(w, heap, reorder, hyps, a)?}
      }
      ProofNode::Unfold {res, ..} => {
        let (l, l2, c) = &**res;
        self.write_proof(w, heap, reorder, hyps, l, false)?;
        self.write_proof(w, heap, reorder, hyps, l2, false)?;
        ProofCmd::Unfold.write_to(w)?;
        self.write_conv(w, heap, reorder, hyps, c)?;
      }
    }
    Ok(())
  }

  #[inline]
  fn write_thm_header(header: &mut [u8], nargs: u16, p_thm: u32) {
    LE::write_u16(&mut header[0..], nargs);
    LE::write_u32(&mut header[4..], p_thm);
  }

  fn write_index_entry(&mut self, header: &mut IndexHeader<'_>, il: u64, ir: u64,
      (sort, a, cmd): (bool, AtomId, u64)) -> io::Result<u64> {
    let n = self.align_to(8)?;
    let (sp, ix, k, name) = if sort {
      let ad = &self.env.data()[a];
      let s = ad.sort().expect("expected a sort");
      header.sort(s).set(n);
      (&self.env.sort(s).span, s.0.into(), STMT_SORT, ad.name())
    } else {
      let ad = &self.env.data()[a];
      match ad.decl().expect("expected a term/thm") {
        DeclKey::Term(t) => {
          let td = self.env.term(t);
          header.term(t).set(n);
          (&td.span, t.0,
            match td.kind {
              TermKind::Term => STMT_TERM,
              TermKind::Def(_) if td.vis == Modifiers::LOCAL => STMT_DEF | STMT_LOCAL,
              TermKind::Def(_) => STMT_DEF
            },
            ad.name())
        }
        DeclKey::Thm(t) => {
          let td = self.env.thm(t);
          header.thm(t).set(n);
          (&td.span, t.0,
            match td.kind {
              ThmKind::Axiom => STMT_AXIOM,
              ThmKind::Thm(_) if td.vis == Modifiers::PUB => STMT_THM,
              ThmKind::Thm(_) => STMT_THM | STMT_LOCAL
            },
            ad.name())
        }
      }
    };

    let pos = if sp.file.ptr_eq(&self.file) {
      if let Some(src) = self.source {
        src.to_pos(sp.span.start)
      } else {Default::default()}
    } else {Default::default()};
    self.write_u64(il)?;
    self.write_u64(ir)?;
    self.write_u32(pos.line)?;
    self.write_u32(pos.character)?;
    self.write_u64(cmd)?;
    self.write_u32(ix)?;
    self.write_u8(k)?;
    for &c in &**name {assert!(c != 0)}
    self.write_all(name)?;
    self.write_u8(0)?;
    Ok(n)
  }

  fn write_index(&mut self, header: &mut IndexHeader<'_>, left: &[(bool, AtomId, u64)], map: &[(bool, AtomId, u64)]) -> io::Result<u64> {
    #[allow(clippy::integer_division)]
    let mut lo = map.len() / 2;
    let a = match map.get(lo) {
      None => {
        let mut n = 0;
        for &t in left.iter().rev() {
          n = self.write_index_entry(header, 0, n, t)?
        }
        return Ok(n)
      }
      Some(&(_, a, _)) => a
    };
    let mut hi = lo + 1;
    loop {
      match lo.checked_sub(1) {
        Some(i) if map[i].1 == a => lo = i,
        _ => break,
      }
    }
    loop {
      match map.get(hi) {
        Some(k) if k.1 == a => hi += 1,
        _ => break,
      }
    }
    let il = self.write_index(header, left, &map[..lo])?;
    let ir = self.write_index(header, &map[lo+1..hi], &map[hi..])?;
    self.write_index_entry(header, il, ir, map[lo])
  }

  /// Perform the actual export. If `index` is true, also output the
  /// (optional) debugging table to the file.
  ///
  /// This does not finalize all writes. [`finish`] should be called after this
  /// to write the outstanding fixups.
  ///
  /// [`finish`]: Self::finish
  pub fn run(&mut self, index: bool) -> io::Result<()> {
    self.write_all(&MM0B_MAGIC)?; // magic
    let num_sorts = self.env.sorts().len();
    assert!(num_sorts <= 128, "too many sorts (max 128)");
    #[allow(clippy::cast_possible_truncation)]
    self.write_all(&[MM0B_VERSION, num_sorts as u8, 0, 0])?; // two bytes reserved
    let num_terms = self.env.terms().len();
    self.write_u32(num_terms.try_into().expect("too many terms"))?; // num_terms
    let num_thms = self.env.thms().len();
    self.write_u32(num_thms.try_into().expect("too many thms"))?; // num_thms
    let p_terms = self.fixup32()?;
    let p_thms = self.fixup32()?;
    let p_proof = self.fixup64()?;
    let p_index = self.fixup64()?;

    // sort data
    self.write_all(&self.env.sorts().iter().map(|s| s.mods.bits()).collect::<Vec<u8>>())?;

    // term header
    self.align_to(8)?; p_terms.commit(self);
    let mut term_header = self.fixup_large(num_terms * 8)?;
    for (head, t) in term_header.chunks_exact_mut(8).zip(&self.env.terms().0) {
      let nargs: u16 = t.args.len().try_into().expect("term has more than 65536 args");
      Self::write_term_header(head, nargs, t.ret.0,
        matches!(t.kind, TermKind::Def(_)),
        self.align_to(8)?.try_into().expect("address too large"));
      self.write_binders(&t.args)?;
      self.write_sort_deps(false, t.ret.0, t.ret.1)?;
      let reorder = if let TermKind::Def(val) = &t.kind {
        let Expr {heap, head} = val.as_ref().unwrap_or_else(||
          panic!("def {} missing value", self.env.data()[t.atom].name()));
        let mut reorder = Reorder::new(nargs.into(), heap.len(), |i| i);
        self.write_expr_unify(heap, &mut reorder, head, &mut vec![])?;
        self.write_u8(0)?;
        Some(reorder)
      } else { None };
      self.term_reord.push(reorder)
    }
    term_header.commit(self);

    // theorem header
    self.align_to(8)?; p_thms.commit(self);
    let mut thm_header = self.fixup_large(num_thms * 8)?;
    for (head, t) in thm_header.chunks_exact_mut(8).zip(&self.env.thms().0) {
      let nargs = t.args.len().try_into().expect("theorem has more than 65536 args");
      Self::write_thm_header(head, nargs,
        self.align_to(8)?.try_into().expect("address too large"));
      self.write_binders(&t.args)?;
      let mut reorder = Reorder::new(nargs.into(), t.heap.len(), |i| i);
      let save = &mut vec![];
      self.write_expr_unify(&t.heap, &mut reorder, &t.ret, save)?;
      for (_, h) in t.hyps.iter().rev() {
        UnifyCmd::Hyp.write_to(self)?;
        self.write_expr_unify(&t.heap, &mut reorder, h, save)?;
      }
      self.write_u8(0)?;
    }
    thm_header.commit(self);

    // main body (proofs of theorems)
    p_proof.commit(self);
    let vec = &mut vec![];
    let mut index_map = Vec::with_capacity(if index {num_sorts + num_terms + num_thms} else {0});
    for s in self.env.stmts() {
      match *s {
        StmtTrace::Sort(a) => {
          if index {index_map.push((true, a, self.pos))}
          write_cmd_bytes(self, STMT_SORT, &[])?
        }
        StmtTrace::Decl(a) => {
          if index {index_map.push((false, a, self.pos))}
          match self.env.data()[a].decl().expect("expected a term/thm") {
            DeclKey::Term(t) => {
              let td = self.env.term(t);
              match &td.kind {
                TermKind::Term => write_cmd_bytes(self, STMT_TERM, &[])?,
                TermKind::Def(None) => panic!("def {} missing definition", self.env.data()[td.atom].name()),
                TermKind::Def(Some(Expr {heap, head})) => {
                  #[allow(clippy::cast_possible_truncation)] // no truncation
                  let nargs = td.args.len() as u32;
                  let mut reorder = Reorder::new(nargs, heap.len(), |i| i);
                  write_expr_proof(vec, heap, &mut reorder, head, false)?;
                  vec.write_u8(0)?;
                  let cmd = STMT_DEF | if td.vis == Modifiers::LOCAL {STMT_LOCAL} else {0};
                  write_cmd_bytes(self, cmd, vec)?;
                  vec.clear();
                }
              }
            }
            DeclKey::Thm(t) => {
              let td = self.env.thm(t);
              #[allow(clippy::cast_possible_truncation)] // no truncation
              let nargs = td.args.len() as u32;
              let cmd = match &td.kind {
                ThmKind::Axiom => {
                  let mut reorder = Reorder::new(nargs, td.heap.len(), |i| i);
                  for (_, h) in &*td.hyps {
                    write_expr_proof(vec, &td.heap, &mut reorder, h, false)?;
                    ProofCmd::Hyp.write_to(vec)?;
                  }
                  write_expr_proof(vec, &td.heap, &mut reorder, &td.ret, false)?;
                  STMT_AXIOM
                }
                ThmKind::Thm(None) => panic!("proof {} missing", self.env.data()[td.atom].name()),
                ThmKind::Thm(Some(Proof {heap, hyps, head})) => {
                  let mut reorder = Reorder::new(nargs, heap.len(), |i| i);
                  let mut ehyps = Vec::with_capacity(hyps.len());
                  for h in &**hyps {
                    let e = match h.deref(heap) {
                      ProofNode::Hyp(_, ref e) => &**e,
                      _ => unreachable!()
                    };
                    self.write_proof(vec, heap, &mut reorder, &ehyps, e, false)?;
                    ProofCmd::Hyp.write_to(vec)?;
                    ehyps.push(reorder.idx);
                    reorder.idx += 1;
                  }
                  self.write_proof(vec, heap, &mut reorder, &ehyps, head, false)?;
                  STMT_THM | if td.vis == Modifiers::PUB {0} else {STMT_LOCAL}
                }
              };
              vec.write_u8(0)?;
              write_cmd_bytes(self, cmd, vec)?;
              vec.clear();
            }
          }
        }
        StmtTrace::Global(_) |
        StmtTrace::OutputString(_) => {}
      }
    }
    self.write_u8(0)?;

    // debugging index
    if index {
      self.align_to(8)?; p_index.commit(self);
      index_map.sort_unstable_by_key(|k| &**self.env.data()[k.1].name());
      let size = 1 + num_sorts + num_terms + num_thms;
      let mut index_header = self.fixup_large(8 * size)?;
      let header = LayoutVerified::<_, [U64<LE>]>::new_slice_unaligned(&mut *index_header).expect("nonempty");
      let (root, header) = unwrap_unchecked!(header.into_mut_slice().split_first_mut());
      let (sorts, header) = header.split_at_mut(num_sorts);
      let (terms, thms) = header.split_at_mut(num_terms);
      root.set(self.write_index(&mut IndexHeader {sorts, terms, thms}, &[], &index_map)?);
      index_header.commit(self)
    } else {
      p_index.cancel();
      self.write_u32(0)?; // padding
    }
    Ok(())
  }

  /// Finalize the outstanding fixups, and flush the writer. Consumes self since we're done.
  pub fn finish(self) -> io::Result<()> {
    let Self {mut w, fixups, ..} = self;
    for (pos, f) in fixups {
      w.seek(SeekFrom::Start(pos))?;
      match f {
        Value::U32(n) => w.write_all(n.as_bytes())?,
        Value::U64(n) => w.write_all(n.as_bytes())?,
        Value::Box(buf) => w.write_all(&buf)?,
      }
    }
    w.flush()
  }
}