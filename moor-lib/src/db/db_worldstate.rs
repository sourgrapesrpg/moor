use anyhow::Error;
use async_trait::async_trait;
use tracing::debug;
use uuid::Uuid;

use moor_value::util::bitenum::BitEnum;
use moor_value::var::objid::{ObjSet, Objid, NOTHING};
use moor_value::var::variant::Variant;
use moor_value::var::{v_int, v_list, v_objid, Var};

use crate::db::db_message::DbMessage;
use crate::db::{DbTxWorldState, PropDef, VerbDef};
use moor_value::model::objects::{ObjAttrs, ObjFlag};
use moor_value::model::permissions::PermissionsContext;
use moor_value::model::props::{PropAttrs, PropFlag};
use moor_value::model::r#match::{ArgSpec, PrepSpec, VerbArgsSpec};
use moor_value::model::verbs::{BinaryType, VerbAttrs, VerbFlag, VerbInfo};
use moor_value::model::world_state::WorldState;
use moor_value::model::CommitResult;
use moor_value::model::WorldStateError;

// all of this right now is direct-talk to physical DB transaction, and should be fronted by a
// cache.
// the challenge is how to make the cache work with the transactional semantics of the DB and
// runtime.
// bare simple would be a rather inefficient cache that is flushed and re-read for each tx
// better would be one that is long lived and shared with other transactions, but this is far more
// challenging, esp if we want to support a distributed db back-end at some point. in that case,
// the invalidation process would need to be distributed as well.
// there's probably some optimistic scheme that could be done here, but here is my first thought
//    * every tx has a cache
//    * there's also a 'global' cache
//    * the tx keeps track of which entities it has modified. when it goes to commit, those
//      entities are locked.
//    * when a tx commits successfully into the db, the committed changes are merged into the
//      upstream cache, and the lock released
//    * if a tx commit fails, the (local) changes are discarded, and, again, the lock released
//    * likely something that should get run through Jepsen

fn verbhandle_to_verbinfo(vh: &VerbDef, program: Option<Vec<u8>>) -> VerbInfo {
    VerbInfo {
        names: vh.names.clone(),
        attrs: VerbAttrs {
            definer: Some(vh.location),
            owner: Some(vh.owner),
            flags: Some(vh.flags),
            args_spec: Some(vh.args),
            binary_type: vh.binary_type,
            binary: program,
        },
    }
}

fn prophandle_to_propattrs(ph: &PropDef, value: Option<Var>) -> PropAttrs {
    PropAttrs {
        name: Some(ph.name.clone()),
        value,
        location: Some(ph.location),
        owner: Some(ph.owner),
        flags: Some(ph.perms),
    }
}

#[async_trait]
impl WorldState for DbTxWorldState {
    #[tracing::instrument(skip(self))]
    async fn owner_of(&mut self, obj: Objid) -> Result<Objid, WorldStateError> {
        let (send, receive) = tokio::sync::oneshot::channel();
        self.mailbox
            .send(DbMessage::GetObjectOwner(obj, send))
            .expect("Error sending message");
        let oid = receive.await.expect("Error receiving message")?;
        Ok(oid)
    }

    #[tracing::instrument(skip(self))]
    async fn flags_of(&mut self, obj: Objid) -> Result<BitEnum<ObjFlag>, WorldStateError> {
        let (send, receive) = tokio::sync::oneshot::channel();
        self.mailbox
            .send(DbMessage::GetObjectFlagsOf(obj, send))
            .expect("Error sending message");
        let flags = receive.await.expect("Error receiving message")?;
        Ok(flags)
    }

    async fn set_flags_of(
        &mut self,
        perms: PermissionsContext,
        obj: Objid,
        new_flags: BitEnum<ObjFlag>,
    ) -> Result<(), Error> {
        // Owner or wizard only.
        let (flags, owner) = (self.flags_of(obj).await?, self.owner_of(obj).await?);
        perms
            .task_perms()
            .check_object_allows(owner, flags, ObjFlag::Write)?;
        let (send, receive) = tokio::sync::oneshot::channel();
        self.mailbox
            .send(DbMessage::SetObjectFlagsOf(obj, new_flags, send))
            .expect("Error sending message");
        receive.await.expect("Error receiving message")?;
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    async fn location_of(
        &mut self,
        perms: PermissionsContext,
        obj: Objid,
    ) -> Result<Objid, WorldStateError> {
        let (flags, owner) = (self.flags_of(obj).await?, self.owner_of(obj).await?);
        perms
            .task_perms()
            .check_object_allows(owner, flags, ObjFlag::Read)?;

        let (send, receive) = tokio::sync::oneshot::channel();
        self.mailbox
            .send(DbMessage::GetLocationOf(obj, send))
            .expect("Error sending message");
        let oid = receive.await.expect("Error receiving message")?;
        Ok(oid)
    }

    #[tracing::instrument(skip(self))]
    async fn create_object(
        &mut self,
        perms: PermissionsContext,
        parent: Objid,
        owner: Objid,
    ) -> Result<Objid, WorldStateError> {
        let (flags, parent_owner) = (self.flags_of(parent).await?, self.owner_of(parent).await?);
        // TODO check_object_allows should take a BitEnum arg for `allows` and do both of these at
        // once.
        perms
            .task_perms()
            .check_object_allows(parent_owner, flags, ObjFlag::Read)?;
        perms
            .task_perms()
            .check_object_allows(parent_owner, flags, ObjFlag::Fertile)?;

        let owner = (owner != NOTHING).then_some(owner);

        /*
            TODO: quota:
            If the intended owner of the new object has a property named `ownership_quota' and the value of that property is an integer, then `create()' treats that value
            as a "quota".  If the quota is less than or equal to zero, then the quota is considered to be exhausted and `create()' raises `E_QUOTA' instead of creating an
            object.  Otherwise, the quota is decremented and stored back into the `ownership_quota' property as a part of the creation of the new object.
        */

        let attrs = ObjAttrs {
            owner,
            name: None,
            parent: Some(parent),
            location: None,
            flags: None,
        };
        let (send, receive) = tokio::sync::oneshot::channel();
        self.mailbox
            .send(DbMessage::CreateObject {
                id: None,
                attrs,
                reply: send,
            })
            .expect("Error sending message");
        let oid = receive.await.expect("Error receiving message")?;
        Ok(oid)
    }

    async fn move_object(
        &mut self,
        perms: PermissionsContext,
        obj: Objid,
        new_loc: Objid,
    ) -> Result<(), WorldStateError> {
        let (flags, owner) = (self.flags_of(obj).await?, self.owner_of(obj).await?);
        perms
            .task_perms()
            .check_object_allows(owner, flags, ObjFlag::Write)?;

        let (send, receive) = tokio::sync::oneshot::channel();
        self.mailbox
            .send(DbMessage::SetLocationOf(obj, new_loc, send))
            .expect("Error sending message");
        receive.await.expect("Error receiving message")?;
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    async fn contents_of(
        &mut self,
        perms: PermissionsContext,
        obj: Objid,
    ) -> Result<ObjSet, WorldStateError> {
        let (flags, owner) = (self.flags_of(obj).await?, self.owner_of(obj).await?);
        perms
            .task_perms()
            .check_object_allows(owner, flags, ObjFlag::Read)?;

        let (send, receive) = tokio::sync::oneshot::channel();
        self.mailbox
            .send(DbMessage::GetContentsOf(obj, send))
            .expect("Error sending message");
        let contents = receive.await.expect("Error receiving message")?;
        Ok(contents)
    }

    #[tracing::instrument(skip(self))]
    async fn verbs(
        &mut self,
        perms: PermissionsContext,
        obj: Objid,
    ) -> Result<Vec<VerbInfo>, WorldStateError> {
        let (flags, owner) = (self.flags_of(obj).await?, self.owner_of(obj).await?);
        perms
            .task_perms()
            .check_object_allows(owner, flags, ObjFlag::Read)?;

        let (send, receive) = tokio::sync::oneshot::channel();
        self.mailbox
            .send(DbMessage::GetVerbs(obj, send))
            .expect("Error sending message");
        let verbs = receive.await.expect("Error receiving message")?;
        Ok(verbs
            .iter()
            .map(|vh| {
                // TODO: is definer correct here? I forget if MOO has a Cold-like definer-is-not-location concept
                verbhandle_to_verbinfo(vh, None)
            })
            .collect())
    }

    #[tracing::instrument(skip(self))]
    async fn properties(
        &mut self,
        perms: PermissionsContext,
        obj: Objid,
    ) -> Result<Vec<(String, PropAttrs)>, WorldStateError> {
        let (flags, owner) = (self.flags_of(obj).await?, self.owner_of(obj).await?);
        perms
            .task_perms()
            .check_object_allows(owner, flags, ObjFlag::Read)?;

        let (send, receive) = tokio::sync::oneshot::channel();
        self.mailbox
            .send(DbMessage::GetProperties(obj, send))
            .expect("Error sending message");
        let properties = receive.await.expect("Error receiving message")?;
        Ok(properties
            .iter()
            .filter_map(|ph| {
                // Filter out anything that isn't directly defined on us.
                if ph.definer != obj {
                    return None;
                }
                Some((ph.name.clone(), prophandle_to_propattrs(ph, None)))
            })
            .collect())
    }

    #[tracing::instrument(skip(self))]
    async fn retrieve_property(
        &mut self,
        perms: PermissionsContext,
        obj: Objid,
        pname: &str,
    ) -> Result<Var, WorldStateError> {
        if obj == NOTHING || !self.valid(obj).await? {
            return Err(WorldStateError::ObjectNotFound(obj));
        }

        // Special properties like namnne, location, and contents get treated specially.
        if pname == "name" {
            return self
                .names_of(perms, obj)
                .await
                .map(|(name, _)| Var::from(name));
        } else if pname == "location" {
            return self.location_of(perms, obj).await.map(Var::from);
        } else if pname == "contents" {
            let contents = self
                .contents_of(perms, obj)
                .await?
                .iter()
                .map(|o| v_objid(*o))
                .collect();
            return Ok(v_list(contents));
        } else if pname == "owner" {
            return self.owner_of(obj).await.map(Var::from);
        } else if pname == "programmer" {
            // TODO these can be set, too.
            let flags = self.flags_of(obj).await?;
            return if flags.contains(ObjFlag::Programmer) {
                Ok(v_int(1))
            } else {
                Ok(v_int(0))
            };
        } else if pname == "wizard" {
            let flags = self.flags_of(obj).await?;
            return if flags.contains(ObjFlag::Wizard) {
                Ok(v_int(1))
            } else {
                Ok(v_int(0))
            };
        }

        let (send, receive) = tokio::sync::oneshot::channel();
        self.mailbox
            .send(DbMessage::ResolveProperty(obj, pname.into(), send))
            .expect("Error sending message");
        let (ph, value) = receive.await.expect("Error receiving message")?;

        perms
            .task_perms()
            .check_property_allows(ph.owner, ph.perms, PropFlag::Read)?;

        Ok(value)
    }

    async fn get_property_info(
        &mut self,
        perms: PermissionsContext,
        obj: Objid,
        pname: &str,
    ) -> Result<PropAttrs, WorldStateError> {
        let (send, receive) = tokio::sync::oneshot::channel();
        self.mailbox
            .send(DbMessage::GetProperties(obj, send))
            .expect("Error sending message");
        let properties = receive.await.expect("Error receiving message")?;
        let ph = properties
            .iter()
            .find(|ph| ph.name == pname)
            .ok_or(WorldStateError::PropertyNotFound(obj, pname.into()))?;

        perms
            .task_perms()
            .check_property_allows(ph.owner, ph.perms, PropFlag::Read)?;

        let attrs = prophandle_to_propattrs(ph, None);
        Ok(attrs)
    }

    async fn set_property_info(
        &mut self,
        perms: PermissionsContext,
        obj: Objid,
        pname: &str,
        attrs: PropAttrs,
    ) -> Result<(), WorldStateError> {
        let (send, receive) = tokio::sync::oneshot::channel();
        self.mailbox
            .send(DbMessage::GetProperties(obj, send))
            .expect("Error sending message");
        let properties = receive.await.expect("Error receiving message")?;
        let ph = properties
            .iter()
            .find(|ph| ph.name == pname)
            .ok_or(WorldStateError::PropertyNotFound(obj, pname.into()))?;

        perms
            .task_perms()
            .check_property_allows(ph.owner, ph.perms, PropFlag::Write)?;

        // Also keep a close eye on 'clear':
        //  "raises `E_INVARG' if <owner> is not valid" & If <object> is the definer of the property
        //   <prop-name>, as opposed to an inheritor of the property, then `clear_property()' raises
        //   `E_INVARG'

        let (send, receive) = tokio::sync::oneshot::channel();
        self.mailbox
            .send(DbMessage::SetPropertyInfo {
                obj,
                uuid: Uuid::from_bytes(ph.uuid),
                new_owner: attrs.owner,
                new_flags: attrs.flags,
                new_name: attrs.name,
                reply: send,
            })
            .expect("Error sending message");
        receive.await.expect("Error receiving message")?;
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    async fn update_property(
        &mut self,
        perms: PermissionsContext,
        obj: Objid,
        pname: &str,
        value: &Var,
    ) -> Result<(), WorldStateError> {
        // You have to use move/chparent for this kinda fun.
        if pname == "location" || pname == "contents" || pname == "parent" || pname == "children" {
            return Err(WorldStateError::PropertyPermissionDenied);
        }

        if pname == "name" || pname == "owner" {
            let (flags, objowner) = (self.flags_of(obj).await?, self.owner_of(obj).await?);
            // User is either wizard or owner
            perms
                .task_perms()
                .check_object_allows(objowner, flags, ObjFlag::Write)?;
            if pname == "name" {
                let Variant::Str(name) = value.variant() else {
                    return Err(WorldStateError::PropertyTypeMismatch);
                };
                let (send, receive) = tokio::sync::oneshot::channel();
                self.mailbox
                    .send(DbMessage::SetObjectNameOf(
                        obj,
                        name.as_str().to_string(),
                        send,
                    ))
                    .expect("Error sending message");
                receive.await.expect("Error receiving message")?;
                return Ok(());
            }

            if pname == "owner" {
                let Variant::Obj(owner) = value.variant() else {
                    return Err(WorldStateError::PropertyTypeMismatch);
                };
                let (send, receive) = tokio::sync::oneshot::channel();
                self.mailbox
                    .send(DbMessage::SetObjectOwner(obj, *owner, send))
                    .expect("Error sending message");
                receive.await.expect("Error receiving message")?;
                return Ok(());
            }
        }

        if pname == "programmer" || pname == "wizard" {
            // Caller *must* be a wizard for either of these.
            perms.task_perms().check_wizard()?;

            // Gott get and then set flags
            let mut flags = self.flags_of(obj).await?;
            if pname == "programmer" {
                flags.set(ObjFlag::Programmer);
            } else if pname == "wizard" {
                flags.set(ObjFlag::Wizard);
            }

            let (send, receive) = tokio::sync::oneshot::channel();
            self.mailbox
                .send(DbMessage::SetObjectFlagsOf(obj, flags, send))
                .expect("Error sending message");
            receive.await.expect("Error receiving message")?;
            return Ok(());
        }

        let (send, receive) = tokio::sync::oneshot::channel();
        self.mailbox
            .send(DbMessage::GetProperties(obj, send))
            .expect("Error sending message");
        let properties = receive.await.expect("Error receiving message")?;
        let ph = properties
            .iter()
            .find(|ph| ph.name == pname)
            .ok_or(WorldStateError::PropertyNotFound(obj, pname.into()))?;

        perms
            .task_perms()
            .check_property_allows(ph.owner, ph.perms, PropFlag::Write)?;

        let (send, receive) = tokio::sync::oneshot::channel();
        self.mailbox
            .send(DbMessage::SetProperty(
                ph.location,
                Uuid::from_bytes(ph.uuid),
                value.clone(),
                send,
            ))
            .expect("Error sending message");
        receive.await.expect("Error receiving message")?;
        Ok(())
    }

    async fn is_property_clear(
        &mut self,
        _perms: PermissionsContext,
        obj: Objid,
        pname: &str,
    ) -> Result<bool, WorldStateError> {
        let (send, receive) = tokio::sync::oneshot::channel();
        self.mailbox
            .send(DbMessage::GetProperties(obj, send))
            .expect("Error sending message");
        let properties = receive.await.expect("Error receiving message")?;
        let ph = properties
            .iter()
            .find(|ph| ph.name == pname)
            .ok_or(WorldStateError::PropertyNotFound(obj, pname.into()))?;

        // Now RetrieveProperty and if it's not there, it's clear.
        let (send, receive) = tokio::sync::oneshot::channel();
        self.mailbox
            .send(DbMessage::RetrieveProperty(
                ph.location,
                Uuid::from_bytes(ph.uuid),
                send,
            ))
            .expect("Error sending message");
        let result = receive.await.expect("Error receiving message");
        // What we want is an ObjectError::PropertyNotFound, that will tell us if it's clear.
        let is_clear = match result {
            Err(WorldStateError::PropertyNotFound(_, _)) => true,
            Ok(_) => false,
            Err(e) => return Err(e),
        };
        Ok(is_clear)
    }

    async fn clear_property(
        &mut self,
        _perms: PermissionsContext,
        obj: Objid,
        pname: &str,
    ) -> Result<(), WorldStateError> {
        // This is just deleting the local *value* portion of the property.
        // First seek the property handle.
        let (send, receive) = tokio::sync::oneshot::channel();
        self.mailbox
            .send(DbMessage::GetProperties(obj, send))
            .expect("Error sending message");
        let properties = receive.await.expect("Error receiving message")?;
        let ph = properties
            .iter()
            .find(|ph| ph.name == pname)
            .ok_or(WorldStateError::PropertyNotFound(obj, pname.into()))?;
        // Then ask the db to remove the value.
        let (send, receive) = tokio::sync::oneshot::channel();
        self.mailbox
            .send(DbMessage::ClearProperty(
                ph.location,
                Uuid::from_bytes(ph.uuid),
                send,
            ))
            .expect("Error sending message");
        receive.await.expect("Error receiving message")?;
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    async fn define_property(
        &mut self,
        perms: PermissionsContext,
        definer: Objid,
        location: Objid,
        pname: &str,
        propowner: Objid,
        prop_flags: BitEnum<PropFlag>,
        initial_value: Option<Var>,
    ) -> Result<(), WorldStateError> {
        // Perms needs to be wizard, or have write permission on object *and* the owner in prop_flags
        // must be the perms
        let (flags, objowner) = (
            self.flags_of(location).await?,
            self.owner_of(location).await?,
        );
        perms
            .task_perms()
            .check_object_allows(objowner, flags, ObjFlag::Write)?;
        perms.task_perms().check_obj_owner_perms(propowner)?;

        let (send, receive) = tokio::sync::oneshot::channel();
        self.mailbox
            .send(DbMessage::DefineProperty {
                definer,
                location,
                name: pname.into(),
                owner: propowner,
                perms: prop_flags,
                value: initial_value,
                reply: send,
            })
            .expect("Error sending message");
        receive.await.expect("Error receiving message")?;
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    async fn add_verb(
        &mut self,
        perms: PermissionsContext,
        obj: Objid,
        names: Vec<String>,
        _owner: Objid,
        flags: BitEnum<VerbFlag>,
        args: VerbArgsSpec,
        binary: Vec<u8>,
        binary_type: BinaryType,
    ) -> Result<(), WorldStateError> {
        let (objflags, owner) = (self.flags_of(obj).await?, self.owner_of(obj).await?);
        perms
            .task_perms()
            .check_object_allows(owner, objflags, ObjFlag::Write)?;

        let (send, receive) = tokio::sync::oneshot::channel();
        self.mailbox
            .send(DbMessage::AddVerb {
                location: obj,
                owner,
                names,
                binary_type,
                binary,
                flags,
                args,
                reply: send,
            })
            .expect("Error sending message");
        receive.await.expect("Error receiving message")?;
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    async fn remove_verb(
        &mut self,
        perms: PermissionsContext,
        obj: Objid,
        vname: &str,
    ) -> Result<(), WorldStateError> {
        let (objflags, owner) = (self.flags_of(obj).await?, self.owner_of(obj).await?);
        perms
            .task_perms()
            .check_object_allows(owner, objflags, ObjFlag::Write)?;

        // Find the verb uuid & permissions.
        let (send, receive) = tokio::sync::oneshot::channel();
        self.mailbox
            .send(DbMessage::GetVerbByName(obj, vname.to_string(), send))
            .expect("Error sending message");
        let vh = receive.await.expect("Error receiving message")?;

        perms
            .task_perms()
            .check_verb_allows(vh.owner, vh.flags, VerbFlag::Write)?;

        let (send, receive) = tokio::sync::oneshot::channel();
        self.mailbox
            .send(DbMessage::DeleteVerb {
                location: obj,
                uuid: Uuid::from_bytes(vh.uuid),
                reply: send,
            })
            .expect("Error sending message");
        receive.await.expect("Error receiving message")?;
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    async fn set_verb_info(
        &mut self,
        perms: PermissionsContext,
        obj: Objid,
        vname: &str,
        owner: Option<Objid>,
        names: Option<Vec<String>>,
        flags: Option<BitEnum<VerbFlag>>,
        args: Option<VerbArgsSpec>,
    ) -> Result<(), WorldStateError> {
        let (send, receive) = tokio::sync::oneshot::channel();
        self.mailbox
            .send(DbMessage::GetVerbByName(obj, vname.to_string(), send))
            .expect("Error sending message");
        let vh = receive.await.expect("Error receiving message")?;

        perms
            .task_perms()
            .check_verb_allows(vh.owner, vh.flags, VerbFlag::Write)?;
        let (send, receive) = tokio::sync::oneshot::channel();
        self.mailbox
            .send(DbMessage::SetVerbInfo {
                obj,
                uuid: Uuid::from_bytes(vh.uuid),
                owner,
                names,
                flags,
                args,
                reply: send,
            })
            .expect("Error sending message");
        receive.await.expect("Error receiving message")?;
        Ok(())
    }

    async fn set_verb_info_at_index(
        &mut self,
        perms: PermissionsContext,
        obj: Objid,
        vidx: usize,
        owner: Option<Objid>,
        names: Option<Vec<String>>,
        flags: Option<BitEnum<VerbFlag>>,
        args: Option<VerbArgsSpec>,
    ) -> Result<(), WorldStateError> {
        let (send, receive) = tokio::sync::oneshot::channel();
        self.mailbox
            .send(DbMessage::GetVerbs(obj, send))
            .expect("Error sending message");
        let verbs = receive.await.expect("Error receiving message")?;
        if vidx >= verbs.len() {
            return Err(WorldStateError::VerbNotFound(obj, format!("{}", vidx)));
        }
        let vh = verbs[vidx].clone();
        perms
            .task_perms()
            .check_verb_allows(vh.owner, vh.flags, VerbFlag::Write)?;
        let (send, receive) = tokio::sync::oneshot::channel();
        self.mailbox
            .send(DbMessage::SetVerbInfo {
                obj,
                uuid: Uuid::from_bytes(vh.uuid),
                owner,
                names,
                flags,
                args,
                reply: send,
            })
            .expect("Error sending message");
        receive.await.expect("Error receiving message")?;
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    async fn get_verb(
        &mut self,
        perms: PermissionsContext,
        obj: Objid,
        vname: &str,
    ) -> Result<VerbInfo, WorldStateError> {
        let (send, receive) = tokio::sync::oneshot::channel();
        self.mailbox
            .send(DbMessage::GetVerbByName(obj, vname.to_string(), send))
            .expect("Error sending message");
        let vh = receive.await.expect("Error receiving message")?;

        perms
            .task_perms()
            .check_verb_allows(vh.owner, vh.flags, VerbFlag::Read)?;

        let (send, receive) = tokio::sync::oneshot::channel();
        self.mailbox
            .send(DbMessage::GetVerbBinary(
                vh.location,
                Uuid::from_bytes(vh.uuid),
                send,
            ))
            .expect("Error sending message");
        let binary = receive.await.expect("Error receiving message")?;
        Ok(verbhandle_to_verbinfo(&vh, Some(binary)))
    }

    async fn get_verb_at_index(
        &mut self,
        perms: PermissionsContext,
        obj: Objid,
        vidx: usize,
    ) -> Result<VerbInfo, WorldStateError> {
        let (send, receive) = tokio::sync::oneshot::channel();
        self.mailbox
            .send(DbMessage::GetVerbByIndex(obj, vidx, send))
            .expect("Error sending message");
        let vh = receive.await.expect("Error receiving message")?;

        perms
            .task_perms()
            .check_verb_allows(vh.owner, vh.flags, VerbFlag::Read)?;

        let (send, receive) = tokio::sync::oneshot::channel();
        self.mailbox
            .send(DbMessage::GetVerbBinary(
                vh.location,
                Uuid::from_bytes(vh.uuid),
                send,
            ))
            .expect("Error sending message");
        let binary = receive.await.expect("Error receiving message")?;
        Ok(verbhandle_to_verbinfo(&vh, Some(binary)))
    }

    #[tracing::instrument(skip(self))]
    async fn find_method_verb_on(
        &mut self,
        perms: PermissionsContext,
        obj: Objid,
        vname: &str,
    ) -> Result<VerbInfo, WorldStateError> {
        // We were mistakenly doing a perms check on the object itself.  turns out that it's the
        // verbthat purely determenis permsisions.
        let (send, receive) = tokio::sync::oneshot::channel();
        self.mailbox
            .send(DbMessage::ResolveVerb(obj, vname.to_string(), None, send))
            .expect("Error sending message");
        let vh = receive.await.expect("Error receiving message")?;

        perms
            .task_perms()
            .check_verb_allows(vh.owner, vh.flags, VerbFlag::Read)?;

        let (send, receive) = tokio::sync::oneshot::channel();
        self.mailbox
            .send(DbMessage::GetVerbBinary(
                vh.location,
                Uuid::from_bytes(vh.uuid),
                send,
            ))
            .expect("Error sending message");
        let binary = receive.await.expect("Error receiving message")?;
        Ok(verbhandle_to_verbinfo(&vh, Some(binary)))
    }

    #[tracing::instrument(skip(self))]
    async fn find_command_verb_on(
        &mut self,
        perms: PermissionsContext,
        obj: Objid,
        command_verb: &str,
        dobj: Objid,
        prep: PrepSpec,
        iobj: Objid,
    ) -> Result<Option<VerbInfo>, WorldStateError> {
        if !self.valid(obj).await? {
            return Ok(None);
        }

        let (objflags, owner) = (self.flags_of(obj).await?, self.owner_of(obj).await?);
        perms
            .task_perms()
            .check_object_allows(owner, objflags, ObjFlag::Read)?;

        let spec_for_fn = |oid, pco| -> ArgSpec {
            if pco == oid {
                ArgSpec::This
            } else if pco == NOTHING {
                ArgSpec::None
            } else {
                ArgSpec::Any
            }
        };

        let dobj = spec_for_fn(obj, dobj);
        let iobj = spec_for_fn(obj, iobj);
        let argspec = VerbArgsSpec { dobj, prep, iobj };

        let (send, receive) = tokio::sync::oneshot::channel();
        self.mailbox
            .send(DbMessage::ResolveVerb(
                obj,
                command_verb.to_string(),
                Some(argspec),
                send,
            ))
            .expect("Error sending message");

        let vh = receive.await.expect("Error receiving message");
        let vh = match vh {
            Ok(vh) => vh,
            Err(WorldStateError::VerbNotFound(_, _)) => {
                return Ok(None);
            }
            Err(e) => {
                return Err(e);
            }
        };

        perms
            .task_perms()
            .check_verb_allows(vh.owner, vh.flags, VerbFlag::Read)?;

        let (send, receive) = tokio::sync::oneshot::channel();
        self.mailbox
            .send(DbMessage::GetVerbBinary(
                vh.location,
                Uuid::from_bytes(vh.uuid),
                send,
            ))
            .expect("Error sending message");
        let binary = receive.await.expect("Error receiving message")?;
        Ok(Some(verbhandle_to_verbinfo(&vh, Some(binary))))
    }

    #[tracing::instrument(skip(self))]
    async fn parent_of(
        &mut self,
        _perms: PermissionsContext,
        obj: Objid,
    ) -> Result<Objid, WorldStateError> {
        // TODO: MOO does not check permissions on this. Should it?
        let (send, receive) = tokio::sync::oneshot::channel();
        self.mailbox
            .send(DbMessage::GetParentOf(obj, send))
            .expect("Error sending message");
        let oid = receive.await.expect("Error receiving message")?;
        Ok(oid)
    }

    async fn change_parent(
        &mut self,
        perms: PermissionsContext,
        obj: Objid,
        new_parent: Objid,
    ) -> Result<(), WorldStateError> {
        if obj == new_parent {
            return Err(WorldStateError::RecursiveMove(obj, new_parent));
        }

        let (objflags, owner) = (self.flags_of(obj).await?, self.owner_of(obj).await?);

        if new_parent != NOTHING {
            let (parentflags, parentowner) = (
                self.flags_of(new_parent).await?,
                self.owner_of(new_parent).await?,
            );
            perms
                .task_perms()
                .check_object_allows(parentowner, parentflags, ObjFlag::Write)?;
            perms
                .task_perms()
                .check_object_allows(parentowner, parentflags, ObjFlag::Fertile)?;
        }
        perms
            .task_perms()
            .check_object_allows(owner, objflags, ObjFlag::Write)?;

        let (send, receive) = tokio::sync::oneshot::channel();
        self.mailbox
            .send(DbMessage::SetParent(obj, new_parent, send))
            .expect("Error sending message");
        receive.await.expect("Error receiving message")?;
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    async fn children_of(
        &mut self,
        perms: PermissionsContext,
        obj: Objid,
    ) -> Result<ObjSet, WorldStateError> {
        let (objflags, owner) = (self.flags_of(obj).await?, self.owner_of(obj).await?);
        perms
            .task_perms()
            .check_object_allows(owner, objflags, ObjFlag::Read)?;

        let (send, receive) = tokio::sync::oneshot::channel();
        self.mailbox
            .send(DbMessage::GetChildrenOf(obj, send))
            .expect("Error sending message");
        let children = receive.await.expect("Error receiving message")?;
        debug!("Children: {:?} {:?}", obj, children);
        Ok(children)
    }

    #[tracing::instrument(skip(self))]
    async fn valid(&mut self, obj: Objid) -> Result<bool, WorldStateError> {
        let (send, receive) = tokio::sync::oneshot::channel();
        self.mailbox
            .send(DbMessage::Valid(obj, send))
            .expect("Error sending message");
        let valid = receive.await.expect("Error receiving message");
        Ok(valid)
    }

    #[tracing::instrument(skip(self))]
    async fn names_of(
        &mut self,
        perms: PermissionsContext,
        obj: Objid,
    ) -> Result<(String, Vec<String>), WorldStateError> {
        // Another thing that MOO allows lookup of without permissions.
        let (send, receive) = tokio::sync::oneshot::channel();

        // First get name
        self.mailbox
            .send(DbMessage::GetObjectNameOf(obj, send))
            .expect("Error sending message");
        let name = receive.await.expect("Error receiving message")?;

        // Then grab aliases property.
        let aliases = match self.retrieve_property(perms, obj, "aliases").await {
            Ok(a) => match a.variant() {
                Variant::List(a) => a.iter().map(|v| v.to_string()).collect(),
                _ => {
                    vec![]
                }
            },
            Err(_) => {
                vec![]
            }
        };

        Ok((name, aliases))
    }

    #[tracing::instrument(skip(self))]
    async fn commit(&mut self) -> Result<CommitResult, Error> {
        let (send, receive) = tokio::sync::oneshot::channel();
        self.mailbox.send(DbMessage::Commit(send))?;
        let cr = receive.await?;
        // self.join_handle
        //     .join()
        //     .expect("Error completing transaction");
        Ok(cr)
    }

    #[tracing::instrument(skip(self))]
    async fn rollback(&mut self) -> Result<(), Error> {
        let (send, receive) = tokio::sync::oneshot::channel();
        self.mailbox.send(DbMessage::Rollback(send))?;
        receive.await?;
        // self.join_handle
        //     .join()
        //     .expect("Error rolling back transaction");
        Ok(())
    }
}