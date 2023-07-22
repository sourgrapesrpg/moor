use crate::db::rocksdb::LoaderInterface;
use crate::db::state::{WorldState, WorldStateSource};
use crate::db::CommitResult;
use crate::model::objects::{ObjAttrs, ObjFlag};
use crate::model::props::{PropAttrs, PropFlag};
use crate::model::r#match::VerbArgsSpec;
use crate::model::verbs::{VerbAttrs, VerbFlag, VerbInfo};
use crate::model::ObjectError;
use crate::model::ObjectError::{PropertyNotFound, VerbNotFound};
use crate::tasks::command_parse::ParsedCommand;
use crate::util::bitenum::BitEnum;
use crate::var::{Objid, Var, VAR_NONE};
use crate::vm::opcode::Binary;
use anyhow::Error;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

struct MockStore {
    verbs: HashMap<(Objid, String), VerbInfo>,
    properties: HashMap<(Objid, String), Var>,
}
impl MockStore {
    fn set_verb(&mut self, o: Objid, name: &str, binary: &Binary) {
        self.verbs.insert(
            (o, name.to_string()),
            VerbInfo {
                names: vec![name.to_string()],
                attrs: VerbAttrs {
                    definer: Some(o),
                    owner: Some(o),
                    flags: Some(BitEnum::new_with(VerbFlag::Exec) | VerbFlag::Read),
                    args_spec: Some(VerbArgsSpec::this_none_this()),
                    program: Some(binary.clone()),
                },
            },
        );
    }
}

pub struct MockState(Arc<Mutex<MockStore>>);

impl WorldState for MockState {
    fn location_of(&mut self, _obj: Objid) -> Result<Objid, ObjectError> {
        unimplemented!()
    }

    fn contents_of(&mut self, _obj: Objid) -> Result<Vec<Objid>, ObjectError> {
        unimplemented!()
    }

    fn flags_of(&mut self, _obj: Objid) -> Result<BitEnum<ObjFlag>, ObjectError> {
        Ok(BitEnum::all())
    }

    fn verbs(&mut self, _obj: Objid) -> Result<Vec<VerbInfo>, ObjectError> {
        unimplemented!()
    }

    fn properties(&mut self, _obj: Objid) -> Result<Vec<(String, PropAttrs)>, ObjectError> {
        unimplemented!()
    }

    fn retrieve_property(
        &mut self,
        obj: Objid,
        pname: &str,
        _player_flags: BitEnum<ObjFlag>,
    ) -> Result<Var, ObjectError> {
        let store = self.0.lock().unwrap();
        let p = store.properties.get(&(obj, pname.to_string()));
        match p {
            None => Err(PropertyNotFound(obj, pname.to_string())),
            Some(p) => Ok(p.clone()),
        }
    }

    fn update_property(
        &mut self,
        obj: Objid,
        pname: &str,
        _player_flags: BitEnum<ObjFlag>,
        value: &Var,
    ) -> Result<(), ObjectError> {
        let mut store = self.0.lock().unwrap();
        store
            .properties
            .insert((obj, pname.to_string()), value.clone());
        Ok(())
    }

    fn add_property(
        &mut self,
        obj: Objid,
        pname: &str,
        _owner: Objid,
        _prop_flags: BitEnum<PropFlag>,
        initial_value: Option<Var>,
    ) -> Result<(), ObjectError> {
        let mut store = self.0.lock().unwrap();

        store
            .properties
            .insert((obj, pname.to_string()), initial_value.unwrap_or(VAR_NONE));
        Ok(())
    }

    fn find_method_verb_on(&mut self, obj: Objid, vname: &str) -> Result<VerbInfo, ObjectError> {
        let store = self.0.lock().unwrap();
        let v = store.verbs.get(&(obj, vname.to_string()));
        match v {
            None => Err(VerbNotFound(obj, vname.to_string())),
            Some(v) => Ok(v.clone()),
        }
    }
    fn find_command_verb_on(
        &mut self,
        _oid: Objid,
        _pc: &ParsedCommand,
    ) -> Result<Option<VerbInfo>, ObjectError> {
        unimplemented!()
    }

    fn parent_of(&mut self, _obj: Objid) -> Result<Objid, ObjectError> {
        Ok(Objid(-1))
    }

    fn valid(&mut self, _obj: Objid) -> Result<bool, ObjectError> {
        Ok(true)
    }

    fn names_of(&mut self, _obj: Objid) -> Result<(String, Vec<String>), ObjectError> {
        unimplemented!()
    }

    fn commit(&mut self) -> Result<CommitResult, anyhow::Error> {
        Ok(CommitResult::Success)
    }

    fn rollback(&mut self) -> Result<(), anyhow::Error> {
        Ok(())
    }
}

pub struct MockWorldStateSource(Arc<Mutex<MockStore>>);

impl LoaderInterface for MockWorldStateSource {
    fn create_object(&self, _objid: Option<Objid>, _attrs: &mut ObjAttrs) -> Result<Objid, Error> {
        todo!()
    }

    fn set_object_location(&self, _o: Objid, _location: Objid) -> Result<(), Error> {
        todo!()
    }

    fn add_verb(
        &self,
        _obj: Objid,
        _names: Vec<&str>,
        _owner: Objid,
        _flags: BitEnum<VerbFlag>,
        _args: VerbArgsSpec,
        _binary: Binary,
    ) -> Result<(), Error> {
        todo!()
    }

    fn get_property(&self, _obj: Objid, _pname: &str) -> Result<Option<u128>, Error> {
        todo!()
    }

    fn define_property(
        &self,
        _objid: Objid,
        _propname: &str,
        _owner: Objid,
        _flags: BitEnum<PropFlag>,
        _value: Option<Var>,
    ) -> Result<(), Error> {
        todo!()
    }

    fn set_property(
        &self,
        _objid: Objid,
        _uuid: u128,
        _value: Var,
        _owner: Objid,
        _flags: BitEnum<PropFlag>,
    ) -> Result<(), Error> {
        todo!()
    }

    fn commit(self) -> Result<CommitResult, Error> {
        todo!()
    }
}
impl MockWorldStateSource {
    pub(crate) fn new() -> Self {
        let store = MockStore {
            verbs: Default::default(),
            properties: Default::default(),
        };
        Self(Arc::new(Mutex::new(store)))
    }

    pub fn new_with_verb(name: &str, binary: &Binary) -> Self {
        let mut store = MockStore {
            verbs: Default::default(),
            properties: Default::default(),
        };
        store.set_verb(Objid(0), name, binary);
        Self(Arc::new(Mutex::new(store)))
    }

    pub fn new_with_verbs(verbs: Vec<(&str, &Binary)>) -> Self {
        let mut store = MockStore {
            verbs: Default::default(),
            properties: Default::default(),
        };
        for (v, b) in verbs {
            store.set_verb(Objid(0), v, b);
        }
        Self(Arc::new(Mutex::new(store)))
    }
}

impl WorldStateSource for MockWorldStateSource {
    fn new_world_state(&mut self) -> Result<Box<dyn WorldState>, Error> {
        Ok(Box::new(MockState(self.0.clone())))
    }
}