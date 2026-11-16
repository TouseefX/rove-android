//! Published mechanical defaults; no world cache or material-service overrides.
//! Source: https://raw.githubusercontent.com/Roblox/creator-docs/main/content/en-us/parts/materials.md
//! Acoustic absorption is absent from that table; retain the numeric constructor default.
use super::PhysicalPropertiesValue;

pub(super) fn for_material(value: i64) -> Option<PhysicalPropertiesValue> {
    let values = match value {
        1376 => [2.36, 0.8, 0.2, 0.3, 1.0],    // Asphalt
        788 => [2.691, 0.7, 0.15, 0.3, 1.0],   // Basalt
        848 => [1.922, 0.8, 0.15, 0.3, 1.0],   // Brick
        2304 => [0.7, 0.5, 0.05, 1.0, 2.0],    // Cardboard
        2305 => [1.1, 0.4, 0.25, 1.0, 2.0],    // Carpet
        2306 => [2.4, 0.51, 0.2, 1.0, 1.0],    // CeramicTiles
        2307 => [2.0, 0.51, 0.2, 1.0, 1.0],    // ClayRoofTiles
        880 => [2.691, 0.5, 0.17, 1.0, 1.0],   // Cobblestone
        816 => [2.403, 0.7, 0.2, 0.3, 1.0],    // Concrete
        1040 => [7.85, 0.7, 0.2, 1.0, 1.0],    // CorrodedMetal
        804 => [2.691, 0.65, 0.15, 1.0, 1.0],  // CrackedLava
        1056 => [7.85, 0.35, 0.25, 1.0, 1.0],  // DiamondPlate
        1312 => [0.7, 0.35, 0.05, 1.0, 1.0],   // Fabric
        1072 => [2.7, 0.4, 0.25, 1.0, 1.0],    // Foil
        1584 => [2.403, 0.25, 0.2, 1.0, 1.0],  // ForceField
        1552 => [0.919, 0.05, 0.15, 2.0, 1.0], // Glacier
        1568 => [2.403, 0.25, 0.2, 1.0, 1.0],  // Glass
        832 => [2.691, 0.4, 0.2, 1.0, 1.0],    // Granite
        1280 => [0.9, 0.4, 0.1, 1.0, 1.5],     // Grass
        1360 => [0.9, 0.45, 0.1, 1.0, 1.0],    // Ground
        1536 => [0.919, 0.02, 0.15, 3.0, 1.0], // Ice
        1284 => [0.9, 0.4, 0.1, 2.0, 2.0],     // LeafyGrass
        2309 => [0.86, 0.35, 0.25, 1.0, 1.0],  // Leather
        820 => [2.691, 0.5, 0.15, 1.0, 1.0],   // Limestone
        784 => [2.563, 0.2, 0.17, 1.0, 1.0],   // Marble
        1088 => [7.85, 0.4, 0.25, 1.0, 1.0],   // Metal
        1344 => [0.9, 0.3, 0.07, 3.0, 4.0],    // Mud
        288 => [0.7, 0.3, 0.2, 1.0, 1.0],      // Neon
        836 => [2.691, 0.5, 0.17, 0.3, 1.0],   // Pavement
        864 => [2.403, 0.4, 0.17, 1.0, 1.5],   // Pebble
        2310 => [0.75, 0.6, 0.2, 0.3, 1.0],    // Plaster
        256 => [0.7, 0.3, 0.5, 1.0, 1.0],      // Plastic
        896 => [2.691, 0.5, 0.17, 1.0, 1.0],   // Rock
        2308 => [2.36, 0.8, 0.2, 0.3, 1.0],    // RoofShingles
        2311 => [1.3, 1.5, 0.95, 3.0, 2.0],    // Rubber
        1392 => [2.165, 0.5, 0.05, 1.0, 1.0],  // Salt
        1296 => [1.602, 0.5, 0.05, 5.0, 2.5],  // Sand
        912 => [2.691, 0.5, 0.15, 5.0, 1.0],   // Sandstone
        272 => [0.7, 0.2, 0.5, 1.0, 1.0],      // SmoothPlastic
        800 => [2.691, 0.4, 0.2, 1.0, 1.0],    // Slate
        1328 => [0.9, 0.3, 0.03, 3.0, 4.0],    // Snow
        512 => [0.35, 0.48, 0.2, 1.0, 1.0],    // Wood
        528 => [0.35, 0.48, 0.2, 1.0, 1.0],    // WoodPlanks
        _ => return None,
    };
    PhysicalPropertiesValue::from_arguments(&values).ok()
}
