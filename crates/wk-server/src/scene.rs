//! Host side of `wk:scene`: plugins place real 3D objects (GLB geometry with
//! a live transform) into wk's world, relative to their node's pose. The
//! client renders every registered entity and pushes pointer-ray interactions
//! back into its event queue for the guest to poll.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use wasmtime::component::{HasData, Linker, Resource};
use wasmtime::Result;
use wasmtime_wasi_io::IoView;
use wk_protocol::NodeId;

use crate::plugin::HostState;

wasmtime::component::bindgen!({
    path: "wit-scene",
    world: "scene-host",
    imports: { default: trappable },
    require_store_data_send: true,
    with: {
        "wk:scene/scene.entity": SceneEntity,
    },
});

pub use wk::scene::scene::RayEvent;

static NEXT_ENTITY_ID: AtomicU64 = AtomicU64::new(1);

/// One placed entity, shared between the guest (transform writes, event
/// polling) and the client (rendering, event pushes).
pub struct EntityState {
    /// Stable id — the client keys its GPU mesh cache on this.
    pub id: u64,
    /// The node that owns the entity (its pose is the transform's parent).
    pub node_id: NodeId,
    /// The GLB blob, immutable after construction.
    pub glb: Arc<Vec<u8>>,
    /// Position relative to the node's pose origin.
    pub pos: [f32; 3],
    /// Rotation around +y, radians (composed after the node's yaw).
    pub yaw: f32,
    pub scale: f32,
    /// Pointer-ray interactions queued by the client, drained by the guest.
    pub events: VecDeque<RayEvent>,
}

impl EntityState {
    /// Queue an interaction (bounded so an unread queue can't grow forever).
    pub fn push_event(&mut self, ev: RayEvent) {
        if self.events.len() < 256 {
            self.events.push_back(ev);
        }
    }
}

pub type SharedEntity = Arc<Mutex<EntityState>>;

/// Every live entity across every node, in creation order. Owned by
/// `PluginHost`; the view snapshots it for clients.
pub type SceneRegistry = Arc<Mutex<Vec<SharedEntity>>>;

pub fn new_registry() -> SceneRegistry {
    Arc::new(Mutex::new(Vec::new()))
}

/// The guest-held resource. Removing itself from the registry on drop covers
/// both a guest-initiated drop and the whole store being torn down.
pub struct SceneEntity {
    shared: SharedEntity,
    registry: SceneRegistry,
}

impl Drop for SceneEntity {
    fn drop(&mut self) {
        let mut reg = self.registry.lock().unwrap();
        reg.retain(|e| !Arc::ptr_eq(e, &self.shared));
    }
}

pub fn add_to_linker(l: &mut Linker<HostState>) -> Result<()> {
    wk::scene::scene::add_to_linker::<_, HasScene>(l, |s| s)?;
    Ok(())
}

struct HasScene;
impl HasData for HasScene {
    type Data<'a> = &'a mut HostState;
}

impl wk::scene::scene::Host for HostState {}

impl wk::scene::scene::HostEntity for HostState {
    fn new(&mut self, glb: Vec<u8>) -> Result<Resource<SceneEntity>> {
        let shared = Arc::new(Mutex::new(EntityState {
            id: NEXT_ENTITY_ID.fetch_add(1, Ordering::Relaxed),
            node_id: self.node_id,
            glb: Arc::new(glb),
            pos: [0.0, 0.0, 0.0],
            yaw: 0.0,
            scale: 1.0,
            events: VecDeque::new(),
        }));
        self.scene_reg.lock().unwrap().push(shared.clone());
        let registry = self.scene_reg.clone();
        Ok(self.table().push(SceneEntity { shared, registry })?)
    }

    fn set_position(&mut self, this: Resource<SceneEntity>, x: f32, y: f32, z: f32) -> Result<()> {
        let e = self.table().get(&this)?;
        e.shared.lock().unwrap().pos = [x, y, z];
        Ok(())
    }

    fn set_rotation_y(&mut self, this: Resource<SceneEntity>, radians: f32) -> Result<()> {
        let e = self.table().get(&this)?;
        e.shared.lock().unwrap().yaw = radians;
        Ok(())
    }

    fn set_scale(&mut self, this: Resource<SceneEntity>, s: f32) -> Result<()> {
        let e = self.table().get(&this)?;
        e.shared.lock().unwrap().scale = s;
        Ok(())
    }

    fn poll_event(&mut self, this: Resource<SceneEntity>) -> Result<Option<RayEvent>> {
        let e = self.table().get(&this)?;
        let ev = e.shared.lock().unwrap().events.pop_front();
        Ok(ev)
    }

    fn drop(&mut self, this: Resource<SceneEntity>) -> Result<()> {
        self.table().delete(this)?; // Drop impl deregisters
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropping_an_entity_deregisters_it() {
        let registry = new_registry();
        let shared = Arc::new(Mutex::new(EntityState {
            id: 1,
            node_id: NodeId::nil(),
            glb: Arc::new(Vec::new()),
            pos: [0.0; 3],
            yaw: 0.0,
            scale: 1.0,
            events: VecDeque::new(),
        }));
        registry.lock().unwrap().push(shared.clone());
        let ent = SceneEntity {
            shared,
            registry: registry.clone(),
        };
        assert_eq!(registry.lock().unwrap().len(), 1);
        drop(ent);
        assert_eq!(registry.lock().unwrap().len(), 0);
    }

    #[test]
    fn event_queue_is_bounded() {
        let mut e = EntityState {
            id: 1,
            node_id: NodeId::nil(),
            glb: Arc::new(Vec::new()),
            pos: [0.0; 3],
            yaw: 0.0,
            scale: 1.0,
            events: VecDeque::new(),
        };
        for _ in 0..500 {
            e.push_event(RayEvent::Hover);
        }
        assert_eq!(e.events.len(), 256);
    }
}
