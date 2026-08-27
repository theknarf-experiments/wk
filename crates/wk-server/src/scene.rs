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
    /// Content hash of `glb` — the client keys its GPU mesh cache on this, so
    /// identical geometry uploads once no matter how many entities (or how
    /// many restarts of the owning node) it appears through.
    pub glb_hash: u64,
    /// Position relative to the node's pose origin.
    pub pos: [f32; 3],
    /// Rotation around +y, radians (composed after the node's yaw).
    pub yaw: f32,
    pub scale: f32,
    /// Scenery: geometry that is part of the place, not an object in it. The
    /// client never ray-picks it (so a world-sized entity can't swallow every
    /// click) and takes its presence as "this workspace has a world", which
    /// suppresses the fallback ground plane.
    pub scenery: bool,
    /// Pointer-ray interactions queued by the client, drained by the guest.
    pub events: VecDeque<RayEvent>,
}

/// FNV-1a over the GLB bytes: a cheap content id for the client's mesh cache.
/// Not a security boundary — a collision costs the wrong geometry, and both
/// blobs came from the same host anyway.
pub fn glb_hash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

impl EntityState {
    /// Queue an interaction (bounded so an unread queue can't grow forever).
    /// Scenery is never picked, so it never queues anything.
    pub fn push_event(&mut self, ev: RayEvent) {
        if !self.scenery && self.events.len() < 256 {
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

impl HostState {
    /// Register a new entity, scenery or not, and hand the guest its resource.
    /// The flag is set before the registry sees it: an entity the size of a
    /// plaza must never be visible to the view as something clickable, not
    /// even for the frame between a constructor and a setter.
    fn place(&mut self, glb: Vec<u8>, scenery: bool) -> Result<Resource<SceneEntity>> {
        let shared = Arc::new(Mutex::new(EntityState {
            id: NEXT_ENTITY_ID.fetch_add(1, Ordering::Relaxed),
            node_id: self.node_id,
            glb_hash: glb_hash(&glb),
            glb: Arc::new(glb),
            pos: [0.0, 0.0, 0.0],
            yaw: 0.0,
            scale: 1.0,
            scenery,
            events: VecDeque::new(),
        }));
        self.scene_reg.lock().unwrap().push(shared.clone());
        let registry = self.scene_reg.clone();
        Ok(self.table().push(SceneEntity { shared, registry })?)
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
        self.place(glb, false)
    }

    fn scenery(&mut self, glb: Vec<u8>) -> Result<Resource<SceneEntity>> {
        self.place(glb, true)
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
            glb_hash: 0,
            pos: [0.0; 3],
            yaw: 0.0,
            scale: 1.0,
            scenery: false,
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

    fn bare_entity() -> EntityState {
        EntityState {
            id: 1,
            node_id: NodeId::nil(),
            glb: Arc::new(Vec::new()),
            glb_hash: 0,
            pos: [0.0; 3],
            yaw: 0.0,
            scale: 1.0,
            scenery: false,
            events: VecDeque::new(),
        }
    }

    #[test]
    fn event_queue_is_bounded() {
        let mut e = bare_entity();
        for _ in 0..500 {
            e.push_event(RayEvent::Hover);
        }
        assert_eq!(e.events.len(), 256);
    }

    #[test]
    fn scenery_never_queues_events() {
        // The client shouldn't pick scenery at all, but a stale pick racing
        // the flag must not leave events for a guest that never polls.
        let mut e = bare_entity();
        e.scenery = true;
        e.push_event(RayEvent::Press);
        assert!(e.events.is_empty());
    }

    #[test]
    fn identical_geometry_hashes_alike() {
        // The client's mesh cache dedupes on this: same bytes, same key.
        assert_eq!(glb_hash(b"glTF\x02"), glb_hash(b"glTF\x02"));
        assert_ne!(glb_hash(b"glTF\x02"), glb_hash(b"glTF\x03"));
        assert_ne!(glb_hash(b""), glb_hash(b"\0"));
    }
}
