module MM0.Kernel.Verifier (verify) where

import Control.Monad.Except
import Control.Monad.RWS.Strict
import Data.Word
import Data.List
import Data.Bits
import Data.Char
import Data.Default
import Data.Foldable
import qualified Data.Map.Strict as M
import qualified Data.Sequence as Q
import qualified Data.Set as S
import qualified Data.Text as T
import qualified Data.Binary.Builder as BB
import qualified Data.ByteString.Lazy as B
import qualified Data.ByteString.Lazy.Char8 as BC
import MM0.Kernel.Environment
import MM0.Kernel.Types
import MM0.Util

data VTermData = VTermData {
  _vtArgs :: [PBinder],   -- ^ Arguments
  _vtRet :: DepType,      -- ^ Return value sort
  _vtDef :: Maybe ([(VarName, Sort)], SExpr) } -- ^ Dummies, definition expr

data VThmData = VThmData {
  _vaVars :: [PBinder],      -- ^ Sorts of the variables (bound and regular)
  _vaHyps :: [SExpr],        -- ^ Hypotheses
  _vaRet :: SExpr }          -- ^ Conclusion

-- | Global state of the verifier
data VGlobal = VGlobal {
  -- | Map from sort to sort data
  vSorts :: M.Map Sort SortData,
  -- | Map from TermID to term/def info (for constructing expressions)
  vTerms :: M.Map TermName VTermData,
  -- | Map from ThmID to axiom/theorem info (for instantiating theorems)
  vThms :: M.Map ThmName VThmData,
  -- | The current final segment of the environment that has not yet been checked
  vPos :: [Spec],
  -- | The collection of outputs (for IO)
  vOutput :: Q.Seq B.ByteString }

instance Default VGlobal where
  def = VGlobal def def def def def

type GVerifyM = RWST () (Endo [String]) VGlobal (Either String)

runGVerifyM :: GVerifyM a -> Environment -> Either String (a, Q.Seq B.ByteString)
runGVerifyM m e = do
  (a, st, Endo f) <- runRWST m () def {vPos = toList (eSpec e)}
  guardError "Not all theorems have been proven" (null (vPos st))
  case f [] of
    [] -> return (a, vOutput st)
    ss -> throwError ("errors:\n" ++ concatMap (\s -> s ++ "\n") ss)

report :: a -> GVerifyM a -> GVerifyM a
report a m = catchError m $ \e -> a <$ tell (Endo (e :))

checkNotStrict :: VGlobal -> Sort -> Either String ()
checkNotStrict g t = do
  sd <- fromJustError "sort not found" (vSorts g M.!? t)
  guardError ("cannot bind variable; sort " ++ show t ++ " is strict") (not (sStrict sd))

verify :: B.ByteString -> Environment -> [Stmt] -> Either String (Q.Seq B.ByteString)
verify spectxt env = \p -> snd <$> runGVerifyM (mapM_ verifyCmd p) env where

  verifyCmd :: Stmt -> GVerifyM ()
  verifyCmd (StepSort x) = step >>= \case
    SSort x' sd | x == x' ->
      modify $ \g -> g {vSorts = M.insert x sd (vSorts g)}
    e -> throwError ("incorrect step 'sort " ++ T.unpack x ++ "', found " ++ show e)
  verifyCmd (StepTerm x) = step >>= \case
    SDecl x' (DTerm args ty) | x == x' ->
      modify $ \g -> g {vTerms =
        M.insert x (VTermData args ty Nothing) (vTerms g)}
    e -> throwError ("incorrect step 'term " ++ T.unpack x ++ "', found " ++ show e)
  verifyCmd (StepAxiom x) = step >>= \case
    SDecl x' (DAxiom args hs ret) | x == x' ->
      modify $ \g -> g {vThms =
        M.insert x (VThmData args hs ret) (vThms g)}
    e -> throwError ("incorrect step 'axiom " ++ T.unpack x ++ "', found " ++ show e)
  verifyCmd (StmtDef x vs ret ds defn st) = do
    g <- get
    report () $ withContext x $ lift $ checkDef g vs ret ds defn
    when st $ withContext x $ step >>= \case
      SDecl x' (DDef vs' ret' o) | x == x' ->
        guardError "def does not match declaration" $
          vs == vs' && ret == ret' && case o of
            Nothing -> True
            Just (ds', defn') -> ds == ds' && defn == defn'
      e -> throwError ("incorrect def step, found " ++ show e)
    modify $ \g' -> g' {vTerms =
      M.insert x (VTermData vs ret (Just (ds, defn))) (vTerms g')}
  verifyCmd (StmtThm x vs hs ret ds pf st) = do
    g <- get
    report () $ withContext x $ lift $ checkThm g vs hs ret ds pf
    when st $ withContext x $ step >>= \case
      SThm x' vs' hs' ret' | x == x' ->
        guardError "theorem does not match declaration" $
          vs == vs' && (snd <$> hs) == hs' && ret == ret'
      e -> throwError ("incorrect theorem step, found " ++ show e)
    modify $ \g' -> g' {vThms =
      M.insert x (VThmData vs (snd <$> hs) ret) (vThms g')}
  verifyCmd (StepInout (VIKString out)) = step >>= \case
    SInout (IOKString False e) | not out -> verifyInputString spectxt e
    SInout (IOKString True e) | out -> verifyOutputString e
    _ | out -> throwError "incorrect output step"
    _ -> throwError "incorrect input step"

  step :: GVerifyM Spec
  step = gets vPos >>= \case
    [] -> throwError "nothing more to prove"
    s : ss -> s <$ modify (\g -> g {vPos = ss})

  checkDef :: VGlobal -> [PBinder] -> DepType -> [(VarName, Sort)] ->
    SExpr -> Either String ()
  checkDef g vs (DepType ret rs) ds defn = do
    ctx <- checkBinders g vs M.empty
    guardError "undeclared variable in dependency" $
      all (\r -> any binderBound (ctx M.!? r)) rs
    sd <- fromJustError "sort not found" $ vSorts g M.!? ret
    guardError ("cannot declare term for pure sort " ++ show ret) (not (sPure sd))
    mapM_ (checkNotStrict g . snd) ds
    ctx' <- checkBinders g (uncurry PBound <$> ds) ctx
    (ret', rs') <- defcheckExpr (vTerms g) ctx' defn
    guardError "type error" (ret == ret')
    guardError "unaccounted free variable" $
      S.null (S.difference rs' (S.fromList rs))

  defcheckExpr :: M.Map TermName VTermData -> M.Map VarName PBinder -> SExpr -> Either String (Sort, S.Set VarName)
  defcheckExpr terms ctx = defcheckExpr' where
    defcheckExpr' (SVar v) = case ctx M.!? v of
      Nothing -> throwError "undeclared variable in def expr"
      Just (PBound _ s) -> return (s, S.singleton v)
      Just (PReg _ (DepType s vs)) -> return (s, S.fromList vs)
    defcheckExpr' e@(App t es) = do
      VTermData args (DepType ret rs) _ <- fromJustError "unknown term in def expr" (terms M.!? t)
      (m, ev) <- withContext (T.pack (show e)) $ defcheckArgs args es
      return (ret, ev <> S.fromList ((m M.!) <$> rs))

    defcheckArgs :: [PBinder] -> [SExpr] -> Either String (M.Map VarName VarName, S.Set VarName)
    defcheckArgs args es = go args es M.empty S.empty where
      go [] [] m ev = return (m, ev)
      go (PBound x s : args') (SVar v : es') m ev = case ctx M.!? v of
        Just (PBound _ s') | s == s' ->
          go args' es' (M.insert x v m) ev
        _ -> throwError "non-bound variable in BV slot"
      go (PBound _ _ : _) (_ : _) _ _ =
        throwError "non-bound variable in BV slot"
      go (PReg _ (DepType s vs) : args') (e : es') m ev = do
        (s', ev') <- defcheckExpr' e
        guardError "type mismatch" (s == s')
        let ev'' = foldl' (\ev1 v -> S.delete (m M.! v) ev1) ev' vs
        go args' es' m (ev <> ev'')
      go _ _ _ _ | length args == length es =
        throwError ("term arguments don't match substitutions:" ++
          " args = " ++ show args ++ ", subst = " ++ show es)
      go _ _ _ _ = throwError ("expected " ++ show (length args) ++
        " arguments, got " ++ show (length es))

  checkThm :: VGlobal -> [PBinder] -> [(VarName, SExpr)] -> SExpr ->
    [(VarName, Sort)] -> Proof -> Either String ()
  checkThm g vs hs ret ds pf = do
    ctx <- checkBinders g vs M.empty
    mapM_ (typecheckProvable g ctx . snd) hs
    typecheckProvable g ctx ret
    ctx' <- checkBinders g (uncurry PBound <$> ds) ctx
    ret' <- verifyProof g ctx' (M.fromList hs) pf
    guardError "theorem did not prove what it claimed" (ret == ret')

  typecheckProvable :: VGlobal -> M.Map VarName PBinder -> SExpr -> Either String ()
  typecheckProvable g ctx expr = do
    (s, _, _) <- typecheckExpr (vTerms g) ctx expr
    sd <- fromJustError "sort not found" (vSorts g M.!? s)
    guardError ("non-provable sort " ++ show s ++ " in theorem") (sProvable sd)

  checkBinders :: VGlobal -> [PBinder] -> M.Map VarName PBinder -> Either String (M.Map VarName PBinder)
  checkBinders g = go where
    go [] ctx = return ctx
    go (bi@(PBound v t) : bis) ctx = do
      checkNotStrict g t
      guardError "duplicate variable" $ M.notMember v ctx
      go bis (M.insert v bi ctx)
    go (bi@(PReg v (DepType t ts)) : bis) ctx = do
      guardError "undeclared variable in dependency" $
        all (\v' -> any binderBound (ctx M.!? v')) ts
      guardError "sort not found" (M.member t (vSorts g))
      go bis (M.insert v bi ctx)

typecheckExpr :: M.Map TermName VTermData -> M.Map VarName PBinder ->
  SExpr -> Either String (Sort, Bool, S.Set VarName)
typecheckExpr terms ctx = go where
  go (SVar v) = do
    bi <- fromJustError "undeclared variable in def expr" (ctx M.!? v)
    return (binderSort bi, binderBound bi, S.singleton v)
  go (App t es) = do
    VTermData args (DepType ret _) _ <-
      fromJustError "unknown term in def expr" (terms M.!? t)
    (ret, False,) <$> goArgs args es def

  goArgs :: [PBinder] -> [SExpr] -> S.Set VarName -> Either String (S.Set VarName)
  goArgs [] [] vs = return vs
  goArgs (bi : args) (e : es) vs = do
    (s, b, vs') <- go e
    guardError "type mismatch" (binderSort bi == s)
    when (binderBound bi) $ guardError "non-bound variable in BV slot" b
    goArgs args es (vs <> vs')
  goArgs _ _ _ = throwError "term arguments don't match substitutions"

substExpr :: M.Map VarName SExpr -> SExpr -> SExpr
substExpr subst (SVar v) = subst M.! v
substExpr subst (App t es) = App t (substExpr subst <$> es)

verifyProof :: VGlobal -> M.Map VarName PBinder -> M.Map VarName SExpr ->
  Proof -> Either String SExpr
verifyProof g ctx = verifyProof' where

  verifyProof' :: M.Map VarName SExpr -> Proof -> Either String SExpr
  verifyProof' heap (PHyp h) =
    fromJustError ("subproof " ++ show h ++ " not found") (heap M.!? h)
  verifyProof' heap (PThm t es ps) = do
    VThmData args hs ret <- fromJustError "theorem not found" (vThms g M.!? t)
    subst <- verifyArgs args es
    hs' <- mapM (verifyProof' heap) ps
    guardError "substitution to hypothesis does not match theorem" $
      (substExpr subst <$> hs) == hs'
    return (substExpr subst ret)
  verifyProof' heap (PConv e1 c p) = do
    (e1', e2', _, _) <- verifyConv c
    e2 <- verifyProof' heap p
    guardError "conversion proof mismatch" $ e1 == e1' && e2 == e2'
    return e1
  verifyProof' heap (PLet h p1 p2) = do
    e1 <- verifyProof' heap p1
    guardError ("subproof name shadowing at " ++ show h) $ M.notMember h heap
    verifyProof' (M.insert h e1 heap) p2
  verifyProof' _ PSorry = throwError "? found in proof"

  verifyArgs :: [PBinder] -> [SExpr] -> Either String (M.Map VarName SExpr)
  verifyArgs = go [] where
    go :: [(VarName, (SExpr, (Sort, Bool, S.Set VarName)))] ->
      [PBinder] -> [SExpr] -> Either String (M.Map VarName SExpr)
    go subst [] [] = return $ M.fromList (mapSnd fst <$> subst)
    go subst (bi : bs) (e : es) = do
      p@(s, b, vs) <- typecheckExpr (vTerms g) ctx e
      guardError "type mismatch" (binderSort bi == s)
      case bi of
        PBound _ _ -> do
          guardError "non-bound variable in BV slot" b
          guardError "disjoint variable violation" $
            all (\v -> all (S.notMember v . thd3 . snd . snd) subst) vs
        PReg _ (DepType _ vs') ->
          forM_ subst $ \(_, (_, (_, b', vs1))) -> when b' $
            guardError "disjoint variable violation" $
              all (\v -> elem v vs' || S.notMember v vs) vs1
      go ((binderName bi, (e, p)) : subst) bs es
    go _ _ _ = throwError "argument number mismatch"

  verifyConv :: Conv -> Either String (SExpr, SExpr, Sort, Bool)
  verifyConv (CVar v) = do
    bi <- fromJustError "undeclared dummy in proof" (ctx M.!? v)
    return (SVar v, SVar v, binderSort bi, binderBound bi)
  verifyConv (CApp t cs) = do
    VTermData args (DepType ret _) _ <-
      fromJustError ("unknown term in proof: " ++ show t) (vTerms g M.!? t)
    (es1, es2) <- verifyConvArgs args cs
    return (App t es1, App t es2, ret, False)
  verifyConv (CSym c) = verifyConv c <&> \(e1, e2, s, b) -> (e2, e1, s, b)
  verifyConv (CUnfold t es vs c) = do
    VTermData args _ defn <-
      fromJustError ("unknown term in proof: " ++ show t) (vTerms g M.!? t)
    (ds, val) <- fromJustError ("not a definition: " ++ show t) defn
    guardError "argument number mismatch" $ length args == length es
    subst <- verifyArgs (args ++ (uncurry PBound <$> ds)) (es ++ (SVar <$> vs))
    (e1, e2, s, b) <- verifyConv c
    guardError "conversion proof mismatch" $ e1 == substExpr subst val
    return (App t es, e2, s, b)

  verifyConvArgs :: [PBinder] -> [Conv] -> Either String ([SExpr], [SExpr])
  verifyConvArgs [] [] = return ([], [])
  verifyConvArgs (bi : args) (c : cs) = do
    (e1, e2, s, b) <- verifyConv c
    guardError "type mismatch" (binderSort bi == s)
    when (binderBound bi) $ guardError "non-bound variable in BV slot" b
    (es1, es2) <- verifyConvArgs args cs
    return (e1 : es1, e2 : es2)
  verifyConvArgs _ _ = throwError "term arguments don't match substitutions"

--------------------------------------------------
-- Input/Output for 'string' (optional feature) --
--------------------------------------------------

data StringPart = IFull B.ByteString | IHalf Word8 B.ByteString
type StringInM = StringPart -> Either String StringPart

spUncons :: StringPart -> Maybe (Word8, StringPart)
spUncons (IFull s) = case B.uncons s of
  Nothing -> Nothing
  Just (c, s') -> Just (shiftR c 4, IHalf (c .&. 15) s')
spUncons (IHalf c s) = Just (c, IFull s)

spRest :: StringPart -> B.ByteString
spRest (IFull s) = s
spRest (IHalf _ s) = s

spLen :: StringPart -> Int
spLen (IFull s) = fromIntegral (B.length s)
spLen (IHalf _ s) = fromIntegral (B.length s + 1)

toHex :: Word8 -> Char
toHex i = chr $ (if i < 10 then ord '0' else ord 'a' - 10) + fromIntegral i

verifyInputString :: B.ByteString -> SExpr -> GVerifyM ()
verifyInputString spectxt = \e -> do
  g <- get
  lift $ unify (vTerms g) (M.fromList proclist) e
  where
  proclist :: [(T.Text, (SExpr -> StringInM) -> [SExpr] -> StringInM)]
  proclist =
    ("s0", \_ [] s -> return s) :
    ("s1", \f [e] -> f e) :
    ("sadd", \f [e1, e2] s -> f e1 s >>= f e2) :
    ("ch", \f [e1, e2] s -> f e1 s >>= f e2) :
    map (\i -> (T.pack ('x' : toHex i : []),
      \_ [] s -> case spUncons s of
        Nothing -> throwError "EOF"
        Just (c, s') -> do
          guardError (mismatch s) (c == fromIntegral i)
          return s')) [0..15]

  unify :: M.Map TermName VTermData ->
    M.Map TermName ((SExpr -> StringInM) -> [SExpr] -> StringInM) ->
    SExpr -> Either String ()
  unify terms procs = \e -> go [] e (IFull spectxt) >>= \case
    IFull s | B.null s -> return ()
    s' -> throwError (mismatch s')
    where

    go :: [M.Map VarName SExpr] -> SExpr -> StringInM
    go [] (SVar _) _ = error "free variable found"
    go (es : stk) (SVar v) s = go stk (es M.! v) s
    go stk (App t es) s = case terms M.! t of
      VTermData args _ (Just ([], val)) ->
        go (M.fromList (zip (binderName <$> args) es) : stk) val s
      VTermData _ _ (Just _) ->
        throwError ("definition " ++ show t ++ " has dummy variables")
      VTermData _ _ Nothing -> case procs M.!? t of
        Just f -> f (go stk) es s
        Nothing -> throwError ("term " ++ show t ++ " not supported")

  mismatch s = "input mismatch at char " ++
    show (fromIntegral (B.length spectxt) - spLen s) ++ ": rest = '" ++
      BC.unpack (B.take 10 (spRest s)) ++
      if B.length (spRest s) <= 10 then "'" else "'..."

data OStringPart = OString BB.Builder | OHex Word8
type StringOutM = Either String OStringPart

verifyOutputString :: SExpr -> GVerifyM ()
verifyOutputString = \e -> do
  g <- get
  lift (toString (vTerms g) (M.fromList proclist) e) >>= \case
    OString out -> modify $
      \g' -> g' {vOutput = vOutput g' Q.|> BB.toLazyByteString out}
    OHex _ -> throwError "impossible, check axioms"
  where
  proclist :: [(T.Text, (SExpr -> StringOutM) -> [SExpr] -> StringOutM)]
  proclist =
    ("s0", \_ [] -> return (OString mempty)) :
    ("s1", \f [e] -> f e) :
    ("sadd", \f [e1, e2] ->
      let app (OString s1) (OString s2) = OString (s1 <> s2)
          app _ _ = error "impossible, check axioms" in
      app <$> f e1 <*> f e2) :
    ("ch", \f [e1, e2] ->
      let app (OHex h1) (OHex h2) = OString $ BB.singleton $ shiftL h1 4 .|. h2
          app _ _ = error "impossible, check axioms" in
      app <$> f e1 <*> f e2) :
    map (\i -> (T.pack ('x' : toHex i : []), \_ [] -> return (OHex i))) [0..15]

  toString :: M.Map TermName VTermData ->
    M.Map TermName ((SExpr -> StringOutM) -> [SExpr] -> StringOutM) ->
    SExpr -> StringOutM
  toString terms procs = go [] where
    go :: [M.Map Ident SExpr] -> SExpr -> StringOutM
    go [] (SVar _) = error "free variable found"
    go (es : stk) (SVar v) = go stk (es M.! v)
    go stk (App t es) = case terms M.! t of
      VTermData args _ (Just ([], val)) ->
        go (M.fromList (zip (binderName <$> args) es) : stk) val
      VTermData _ _ (Just _) ->
        throwError ("definition " ++ show t ++ " has dummy variables")
      VTermData _ _ Nothing -> do
        f <- fromJustError ("term " ++ show t ++ " not supported") (procs M.!? t)
        f (go stk) es
