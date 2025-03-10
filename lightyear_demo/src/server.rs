use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};

use bevy::app::PluginGroupBuilder;
use bevy::prelude::*;
use bevy::utils::Duration;
use leafwing_input_manager::prelude::ActionState;

pub use lightyear::prelude::server::*;
use lightyear::prelude::*;

use crate::protocol::*;
use crate::shared::{shared_config, shared_movement_behaviour};
use crate::{shared, ServerTransports, SharedSettings};

const GRID_SIZE: f32 = 200.0;
const VIEW_DISTANCE: i32 = 1; // in grid units (1 = can see 1 grid unit away)
const NUM_CIRCLES: i32 = 10;

// Plugin group to add all server-related plugins
pub struct ServerPluginGroup {
    pub(crate) lightyear: ServerPlugin<MyProtocol>,
}

impl ServerPluginGroup {
    pub(crate) fn new(net_configs: Vec<NetConfig>) -> ServerPluginGroup {
        let config = ServerConfig {
            shared: shared_config(),
            net: net_configs,
            ..default()
        };
        let plugin_config = PluginConfig::new(config, protocol());
        ServerPluginGroup {
            lightyear: ServerPlugin::new(plugin_config),
        }
    }
}

impl PluginGroup for ServerPluginGroup {
    fn build(self) -> PluginGroupBuilder {
        PluginGroupBuilder::start::<Self>()
            .add(self.lightyear)
            .add(ExampleServerPlugin)
            .add(shared::SharedPlugin)
            .add(LeafwingInputPlugin::<MyProtocol, Inputs>::default())
    }
}

// Plugin for server-specific logic
pub struct ExampleServerPlugin;

impl Plugin for ExampleServerPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Global>();
        app.add_systems(Startup, init);
        // the physics/FixedUpdates systems that consume inputs should be run in this set
        app.add_systems(FixedUpdate, movement);
        app.add_systems(
            Update,
            (
                handle_connections,
                interest_management,
                log,
                receive_message,
            ),
        );
    }
}

#[derive(Resource, Default)]
pub(crate) struct Global {
    pub client_id_to_entity_id: HashMap<ClientId, Entity>,
    pub client_id_to_room_id: HashMap<ClientId, RoomId>,
}

pub(crate) fn init(mut commands: Commands) {
    commands.spawn(Camera2dBundle::default());
    commands.spawn(TextBundle::from_section(
        "Server",
        TextStyle {
            font_size: 30.0,
            color: Color::WHITE,
            ..default()
        },
    ));

    // spawn dots in a grid
    for x in -NUM_CIRCLES..NUM_CIRCLES {
        for y in -NUM_CIRCLES..NUM_CIRCLES {
            commands.spawn((
                PlayerId(900 + (NUM_CIRCLES * (x + NUM_CIRCLES) + (y + NUM_CIRCLES)) as u64),
                Position(Vec2::new(x as f32 * GRID_SIZE + GRID_SIZE / 2.0, y as f32 * GRID_SIZE + GRID_SIZE / 2.0)),
                LastPosition(None),
                CircleMarker,
                Replicate {
                    // use rooms for replication
                    replication_mode: ReplicationMode::Room,
                    ..default()
                },
            ));
        }
    }
}

/// Server connection system, create a player upon connection
pub(crate) fn handle_connections(
    mut room_manager: ResMut<RoomManager>,
    mut connections: EventReader<ConnectEvent>,
    mut disconnections: EventReader<DisconnectEvent>,
    mut global: ResMut<Global>,
    mut commands: Commands,
) {
    for connection in connections.read() {
        let client_id = connection.context();
        // Generate pseudo random color from client id.
        let h = (((client_id.wrapping_mul(30)) % 360) as f32) / 360.0;
        let s = 0.8;
        let l = 0.5;
        let entity = commands.spawn(PlayerBundle::new(
            *client_id,
            Vec2::ZERO + Vec2::new(100.0, 100.0),
            Color::hsl(h, s, l),
        ));
        // Add a mapping from client id to entity id (so that when we receive an input from a client,
        // we know which entity to move)
        global
            .client_id_to_entity_id
            .insert(*client_id, entity.id());
    }
    for disconnection in disconnections.read() {
        let client_id = disconnection.context();
        if let Some(entity) = global.client_id_to_entity_id.remove(client_id) {
            commands.entity(entity).despawn();
        }
    }
}

pub(crate) fn log(tick_manager: Res<TickManager>, position: Query<&Position, With<PlayerId>>) {
    let server_tick = tick_manager.tick();
    for pos in position.iter() {
        debug!(?server_tick, "Confirmed position: {:?}", pos);
    }
}

pub(crate) fn receive_message(mut messages: EventReader<MessageEvent<Message1>>) {
    for message in messages.read() {
        info!("recv message");
    }
}

fn get_grid_position(position: Vec2) -> Vec2 {
    Vec2::new(
        (position.x / GRID_SIZE).floor(),
        (position.y / GRID_SIZE).floor(),
    )
}

fn get_room_id_from_grid_position(grid_position: Vec2) -> i64 {
    fn cantor_pairing(a: i64, b: i64) -> i64 {
        (0.5 * (a + b) as f64 * (a + b + 1) as f64 + b as f64) as i64
    }

    fn bijective_map(n: i64) -> i64 {
        if n >= 0 { 2 * n } else { -2 * n - 1 }
    }

    cantor_pairing(bijective_map(grid_position.x as i64), bijective_map(grid_position.y as i64)) as i64
}

// TODO: Remove me.
// Does not work with multiple clients.
static mut DEBUG_ROOM_POSITIONS: Vec<Vec2> = Vec::new();

/// This is where we perform scope management:
/// - we will add/remove other entities from the player's room only if they are close
pub(crate) fn interest_management(
    mut room_manager: ResMut<RoomManager>,
    mut player_query: Query<(&PlayerId, Entity, Ref<Position>, &mut LastPosition)>
) {
    for (client_id, entity, position, last_position) in player_query.iter() {
        if position.is_changed() {
            let grid_position = get_grid_position(position.0);
            match last_position.0 {
                None => {
                    // Add the player to all rooms in the view distance
                    for dx in -VIEW_DISTANCE..=VIEW_DISTANCE {
                        for dy in -VIEW_DISTANCE..=VIEW_DISTANCE {
                            let view_grid_pos = grid_position + Vec2::new(dx as f32, dy as f32);
                            let room_id = get_room_id_from_grid_position(view_grid_pos);
                            unsafe { if client_id.0 < 900 { DEBUG_ROOM_POSITIONS.push(view_grid_pos) } };
                            let mut room = room_manager.room_mut(RoomId(room_id as u16));
                            room.add_client(client_id.0);
                            if dx == 0 && dy == 0 { // Only add the entity to the room if it's in the center grid
                                room.add_entity(entity);
                            }
                            trace!("Player spawned, added to grid_pos {:?} (id: {:?})", view_grid_pos, room_id);
                        }
                    }
                },
                Some(last_position) => {
                    let last_grid_position = get_grid_position(last_position);
                    if grid_position != last_grid_position {
                        let mut last_grid_positions = Vec::new();
                        let mut grid_positions = Vec::new();
                        // Find all grid positions that are in the view distance of the player
                        // as well as grid positions that were in the previous view distance of the player
                        for dx in -VIEW_DISTANCE..=VIEW_DISTANCE {
                            for dy in -VIEW_DISTANCE..=VIEW_DISTANCE {
                                let last_grid_pos = last_grid_position + Vec2::new(dx as f32, dy as f32);
                                let grid_pos = grid_position + Vec2::new(dx as f32, dy as f32);
                                last_grid_positions.push(last_grid_pos);
                                grid_positions.push(grid_pos);
                            }
                        }
                        // Remove the entity from the room it was in before
                        {
                            let room_id = get_room_id_from_grid_position(last_grid_position);
                            let mut room = room_manager.room_mut(RoomId(room_id as u16));
                            room.remove_entity(entity);
                            info!("Entity removed from grid_pos {:?} (id: {:?})", last_grid_position, room_id);
                        }
                        // Add the entity to the room it is in now
                        {
                            let room_id = get_room_id_from_grid_position(grid_position);
                            let mut room = room_manager.room_mut(RoomId(room_id as u16));
                            room.add_entity(entity);
                            info!("Entity added to grid_pos {:?} (id: {:?})", grid_position, room_id);
                        }
                        // Remove the client from rooms that are no longer in view
                        for last_grid_pos in last_grid_positions.iter().filter(|&pos| !grid_positions.contains(pos)) {
                            let room_id = get_room_id_from_grid_position(*last_grid_pos);
                            unsafe { if client_id.0 < 900 { DEBUG_ROOM_POSITIONS.retain(|&pos| pos != *last_grid_pos) } };
                            let mut room = room_manager.room_mut(RoomId(room_id as u16));
                            room.remove_client(client_id.0);
                            trace!("Player removed from grid_pos {:?} (id: {:?})", last_grid_pos, room_id);
                        }
                        // Add the client to rooms that are now in view
                        for grid_pos in grid_positions.iter().filter(|&pos| !last_grid_positions.contains(pos)) {
                            let room_id = get_room_id_from_grid_position(*grid_pos);
                            unsafe { if client_id.0 < 900 { DEBUG_ROOM_POSITIONS.push(*grid_pos) } };
                            let mut room = room_manager.room_mut(RoomId(room_id as u16));
                            room.add_client(client_id.0);
                            trace!("Player added to grid_pos {:?} (id: {:?})", grid_pos, room_id);
                        }
                        unsafe { info!("Player crossed a boundary, new grids: {:?}", DEBUG_ROOM_POSITIONS); }
                    }
                }
            }
        }
    }
    for (client_id, entity, position, mut last_position) in player_query.iter_mut() {
        last_position.0 = Some(position.0);
    }
}

/// Read client inputs and move players
pub(crate) fn movement(mut position_query: Query<(&mut Position, &ActionState<Inputs>)>) {
    for (position, input) in position_query.iter_mut() {
        shared_movement_behaviour(position, input);
    }
}
