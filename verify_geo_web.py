from dbcenter import DBCenter

db = DBCenter("./data")

# Create geo collection (PostGIS auto-loaded)
stores = db.geo_collection("stores")

# Insert locations - simple dict, no SQL!
stores.insert_many([
    {"name": "Store A", "lat": 40.7128, "lon": -74.0060, "city": "NYC"},
    {"name": "Store B", "lat": 34.0522, "lon": -118.2437, "city": "LA"},
    {"name": "Store C", "lat": 40.7589, "lon": -73.9851, "city": "NYC"}
])

# Find nearby - no SQL needed!
nearby = stores.find_within_radius(
    lat=40.7128,
    lon=-74.0060,
    radius_km=5
)

# Distance calculation
distance = stores.distance_between(
    from_id="store_a",
    to_id="store_b"
)

# Check if point is within polygon
is_inside = stores.contains_point(
    polygon_id="delivery_zone_1",
    lat=40.7128,
    lon=-74.0060
)

print("SUCCESS: GeoCollection verification complete.")
print("Nearby stores:", nearby)
print("Distance between Store A and Store B:", distance)
print("Is Store C inside delivery zone?", is_inside)