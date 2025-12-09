from dbcenter import DBCenter
import sys
import os
import shutil
import time

TEST_DIR = os.path.abspath("test_db_debug")
DEBUG_FILE = "debug_output.txt"

def write_debug_info(db_dir):
    with open(DEBUG_FILE, "w") as out:
        out.write("--- PATH Env Var ---\n")
        out.write(os.environ.get("PATH", "") + "\n\n")
        
        # Search for postgis-3.dll
        out.write("--- Searching for postgis-3.dll ---\n")
        home = os.path.expanduser("~")
        dbcenter_dir = os.path.join(home, ".dbcenter")
        if os.path.exists(dbcenter_dir):
            for root, dirs, files in os.walk(dbcenter_dir):
                for file in files:
                    if file.lower() in ["postgis-3.dll", "postgis-3.so", "libgeos.dll", "libgeos.so"]:
                        out.write(f"FOUND: {os.path.join(root, file)}\n")

        # List contents of lib and bin dirs
        if os.path.exists(dbcenter_dir):
            pg_dirs = [d for d in os.listdir(dbcenter_dir) if d.startswith("postgres-")]
            for pg_d in pg_dirs:
                full_d = os.path.join(dbcenter_dir, pg_d)
                lib_d = os.path.join(full_d, "lib")
                bin_d = os.path.join(full_d, "bin")
                share_ext = os.path.join(full_d, "share", "extension")
                
                if os.path.exists(lib_d):
                    out.write(f"\n--- Contents of {lib_d} ---\n")
                    out.write(str(os.listdir(lib_d)) + "\n")

                if os.path.exists(bin_d):
                    out.write(f"\n--- Contents of {bin_d} ---\n")
                    out.write(str(os.listdir(bin_d)) + "\n")
                    
                if os.path.exists(share_ext):
                    out.write(f"\n--- Contents of {share_ext} ---\n")
                    files = os.listdir(share_ext)
                    out.write(str(files) + "\n")
                    
                    if "postgis.control" in files:
                        try:
                            with open(os.path.join(share_ext, "postgis.control"), "r") as f:
                                out.write(f"\n--- postgis.control ---\n{f.read()}\n")
                        except Exception as e:
                            out.write(f"Error reading postgis.control: {e}\n")

                    # Read head of sql files
                    for f in files:
                         if f.startswith("postgis--") and f.endswith(".sql"):
                            try:
                                with open(os.path.join(share_ext, f), "r") as sqlf:
                                    out.write(f"\n--- {f} head ---\n{sqlf.read(500)}\n")
                            except:
                                pass
                            break

        # Log contents
        server_log = os.path.join(db_dir, "server.log")
        if os.path.exists(server_log):
            out.write("\n--- server.log ---\n")
            try:
                with open(server_log, "r") as f:
                    out.write(f.read())
            except:
                pass
                
        pg_conf = os.path.join(db_dir, "postgresql.conf")
        if os.path.exists(pg_conf):
            out.write("\n--- postgresql.conf ---\n")
            try:
                with open(pg_conf, "r") as f:
                    out.write(f.read())
            except:
                pass


# Clean up previous run
if os.path.exists(TEST_DIR):
    try:
        shutil.rmtree(TEST_DIR)
    except:
        pass

db = None
try:
    print(f"Initializing DBCenter at {TEST_DIR}...")
    db = DBCenter(path=TEST_DIR, persist=True, service=False, postgresql=17)
    
    print("Installing PostGIS extension...")
    db.install_extension("postgis")

    versionpostgis = db.query("select PostGIS_Version() as version")
    print(versionpostgis)   
    
    print("SUCCESS: PostGIS installed successfully.")

except Exception as e:
    print(f"FAILURE: {e}")

finally:
    print("Writing debug info...")
    write_debug_info(TEST_DIR)
    
    if db:
        try:
            db.shutdown()
        except:
            pass
