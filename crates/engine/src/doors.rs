//! Load doors: the data half of doors on main. The world database's optional `door_links` table
//! says where a door reference leads, and streaming attaches [`LoadDoor`] to every spawned
//! reference that has a usable link. `crate::door_crossing` activates them.

use crate::world::database::DoorLinkRow;
use bevy::prelude::*;

/// Where a load door leads, as converted from the source door's `XTEL` subrecord.
#[derive(Reflect, Debug, Clone, PartialEq)]
pub struct DoorDestination {
    /// The destination door reference (`XTEL` bytes 0..4, after plugin FormID remapping).
    pub destination_ref_id: u32,
    /// `Some(cell_id)` when the destination is an interior cell.
    pub interior_cell_id: Option<u32>,
    /// `Some(worldspace_id)` when the destination is in a worldspace. Exactly one of
    /// `interior_cell_id` and `worldspace_id` is `Some` for a usable door.
    pub worldspace_id: Option<u32>,
    /// Arrival position in Creation-engine units (`XTEL` bytes 4..16). This is not the destination
    /// door's own position.
    pub arrival_position: [f32; 3],
    /// Arrival rotation in Creation-engine radians (`XTEL` bytes 16..28).
    pub arrival_rotation: [f32; 3],
}

/// Marks a spawned reference as a load door.
#[derive(Component, Reflect, Debug, Clone, PartialEq)]
#[reflect(Component)]
pub struct LoadDoor {
    /// The door reference's own FormID.
    pub ref_id: u32,
    pub destination: DoorDestination,
}

/// The [`LoadDoor`] of a reference with this `door_links` row, or `None` when it has no row or the
/// link's destination cannot be resolved to an interior cell or a worldspace.
pub(crate) fn load_door(ref_id: u32, link: Option<&DoorLinkRow>) -> Option<LoadDoor> {
    let link = link?;
    let interior_cell_id = match (link.destination_worldspace_id, link.destination_cell_id) {
        (None, Some(cell_id)) if cell_id != 0 => Some(cell_id),
        _ => None,
    };
    if interior_cell_id.is_none() && link.destination_worldspace_id.is_none() {
        return None;
    }
    Some(LoadDoor {
        ref_id,
        destination: DoorDestination {
            destination_ref_id: link.destination_ref_id,
            interior_cell_id,
            worldspace_id: link.destination_worldspace_id,
            arrival_position: link.arrival_position,
            arrival_rotation: link.arrival_rotation,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(cell: Option<u32>, worldspace: Option<u32>) -> DoorLinkRow {
        DoorLinkRow {
            destination_ref_id: 7,
            destination_cell_id: cell,
            destination_worldspace_id: worldspace,
            arrival_position: [1.0, 2.0, 3.0],
            arrival_rotation: [0.0, 0.0, 0.5],
        }
    }

    #[test]
    fn an_interior_link_carries_its_cell_and_arrival() {
        let door = load_door(5, Some(&link(Some(99), None))).unwrap();
        assert_eq!(door.ref_id, 5);
        assert_eq!(door.destination.interior_cell_id, Some(99));
        assert_eq!(door.destination.worldspace_id, None);
        assert_eq!(door.destination.arrival_position, [1.0, 2.0, 3.0]);
        assert_eq!(door.destination.arrival_rotation, [0.0, 0.0, 0.5]);
    }

    #[test]
    fn a_worldspace_link_leads_outside() {
        let door = load_door(5, Some(&link(Some(12), Some(60)))).unwrap();
        assert_eq!(door.destination.interior_cell_id, None);
        assert_eq!(door.destination.worldspace_id, Some(60));
    }

    #[test]
    fn an_unresolved_or_absent_link_is_no_load_door() {
        assert!(load_door(5, None).is_none());
        assert!(load_door(5, Some(&link(None, None))).is_none());
        assert!(load_door(5, Some(&link(Some(0), None))).is_none());
    }
}
