use rkyv::{Archive, Deserialize, Serialize};

use crate::compiler::codegen::{JumpLabel, Label};
use crate::compiler::parse::Names;
use crate::model::var::{Var, Offset};

#[derive(Clone, Deserialize, Serialize, Debug, PartialEq, Eq, Archive, Ord, PartialOrd)]
#[archive(
    compare(PartialEq),
    check_bytes,
)]
pub enum ScatterLabel {
    Required(Label),
    Rest(Label),
    Optional(Label, Option<Label>),
}

#[derive(Clone, Deserialize, Serialize, Debug, PartialEq, Eq, Archive, Ord, PartialOrd)]
#[archive(
    compare(PartialEq),
    check_bytes,
)]
pub enum Op {
    If(Label),
    Eif(Label),
    IfQues(Label),
    While(Label),
    Jump {
        label: Label,
    },
    ForList {
        id: Label,
        label: Label,
    },
    ForRange {
        id: Label,
        label: Label,
    },
    Pop,
    Val(Var),
    Imm(Label),
    MkEmptyList,
    ListAddTail,
    ListAppend,
    IndexSet,
    MakeSingletonList,
    CheckListForSplice,
    PutTemp,
    PushTemp,
    Eq,
    Ne,
    Gt,
    Lt,
    Ge,
    Le,
    In,
    Mul,
    Sub,
    Div,
    Mod,
    Add,
    And(Label),
    Or(Label),
    Not,
    UnaryMinus,
    Ref,
    Push(Label),
    PushRef,
    Put(Label),
    RangeRef,
    GPut {
        id: Label,
    },
    GPush {
        id: Label,
    },
    GetProp,
    PushGetProp,
    PutProp,
    Fork {
        f_index: Label,
        id: Option<Label>,
    },
    CallVerb,
    Return,
    Return0,
    Done,
    FuncCall {
        id: Label,
    },
    RangeSet,
    Length(Offset),
    Exp,
    Scatter {
        nargs: Offset,
        nreq: Offset,
        nrest: Offset,
        labels: Vec<ScatterLabel>,
        done: Label,
    },
    PushLabel(Label),
    TryFinally(Label),
    Catch,
    TryExcept(Label),
    EndCatch(Label),
    EndExcept(Label),
    EndFinally,
    WhileId {
        id: Label,
        label: Label,
    },
    Continue,
    ExitId(Label),
    Exit {
        stack: Offset,
        label: Label,
    },
}

/// The result of compilation. The set of instructions, fork vectors, variable offsets, literals.
#[derive(Clone, Deserialize, Serialize, Debug, PartialEq, Archive, Eq, PartialOrd, Ord)]
#[archive(
    compare(PartialEq),
    check_bytes,
)]
pub struct Binary {
    pub(crate) literals: Vec<Var>,
    pub(crate) jump_labels: Vec<JumpLabel>,
    pub(crate) var_names: Names,
    pub(crate) main_vector: Vec<Op>,
    pub(crate) fork_vectors: Vec<Vec<Op>>,
}

impl Binary {
    pub fn new() -> Self {
        Binary {
            literals: Vec::new(),
            jump_labels: Vec::new(),
            var_names: Names::new(),
            main_vector: Vec::new(),
            fork_vectors: Vec::new(),
        }
    }

    pub fn find_var(&self, v: &str) -> Label {
        self.var_names.find_name(v).expect("variable not found").0
    }

    pub fn find_literal(&self, l: Var) -> Label {
        Label(self.literals
            .iter()
            .position(|x| *x == l)
            .expect("literal not found") as u32)
    }
}

impl Default for Binary {
    fn default() -> Self {
        Self::new()
    }
}