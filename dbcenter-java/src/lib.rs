use jni::JNIEnv;
use jni::objects::{JClass, JObject, JString, JValue};
use jni::sys::{jlong, jint, jboolean, jobject};
use dbcenter_core::DBCenter;
use std::sync::Arc;
use tokio::runtime::Runtime;

// Helper to store pointer in Java object (e.g., in a "ptr" long field)
fn set_handle(env: &mut JNIEnv, obj: &JObject, handle: jlong) {
    let class = env.get_object_class(obj).unwrap();
    env.set_field(obj, "ptr", "J", JValue::Long(handle)).unwrap();
}

fn get_handle(env: &mut JNIEnv, obj: &JObject) -> jlong {
    let val = env.get_field(obj, "ptr", "J").unwrap();
    val.j().unwrap()
}

struct JavaDBCenter {
    inner: Arc<DBCenter>,
    runtime: Runtime,
}

#[no_mangle]
pub extern "system" fn Java_com_dbcenter_DBCenter_nativeNew(
    mut env: JNIEnv,
    obj: JObject,
    path: JString,
    persist: jboolean,
) {
    let path: String = env.get_string(&path).expect("Couldn't get java string").into();
    let is_persist = persist != 0;

    let runtime = Runtime::new().unwrap();
    let db = runtime.block_on(async {
        DBCenter::new(Some(path), is_persist, false, None, None, None, None, false)
    });

    match db {
        Ok(d) => {
            let container = Box::new(JavaDBCenter {
                inner: d,
                runtime,
            });
            set_handle(&mut env, &obj, Box::into_raw(container) as jlong);
        }
        Err(e) => {
            env.throw_new("java/lang/RuntimeException", e.to_string()).unwrap();
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_com_dbcenter_DBCenter_nativeFree(
    mut env: JNIEnv,
    obj: JObject,
) {
    let ptr = get_handle(&mut env, &obj);
    if ptr != 0 {
        unsafe {
            let _ = Box::from_raw(ptr as *mut JavaDBCenter);
        }
        set_handle(&mut env, &obj, 0);
    }
}

#[no_mangle]
pub extern "system" fn Java_com_dbcenter_DBCenter_nativeInstallExtension(
    mut env: JNIEnv,
    obj: JObject,
    name: JString,
) {
    let ptr = get_handle(&mut env, &obj);
    if ptr == 0 {
        env.throw_new("java/lang/IllegalStateException", "DBCenter closed").unwrap();
        return;
    }
    let db = unsafe { &*(ptr as *mut JavaDBCenter) };
    
    let name: String = env.get_string(&name).expect("Couldn't get java string").into();
    
    if let Err(e) = db.inner.install_extension(&name) {
         env.throw_new("java/lang/RuntimeException", e.to_string()).unwrap();
    }
}

#[no_mangle]
pub extern "system" fn Java_com_dbcenter_DBCenter_nativeGetVectorStore(
    mut env: JNIEnv,
    obj: JObject,
    name: JString,
    dimension: jint,
) -> jobject {
    let ptr = get_handle(&mut env, &obj);
    if ptr == 0 {
        env.throw_new("java/lang/IllegalStateException", "DBCenter closed").unwrap();
        return JObject::null().into_raw();
    }
    let db = unsafe { &*(ptr as *mut JavaDBCenter) };
    
    let name_str: String = env.get_string(&name).expect("Couldn't get java string").into();
    
    // Create new VectorStore object in Java
    // Assuming com.dbcenter.VectorStore constructor takes (long parentPtr, long ptr, int dim)?
    // Or we modify Java object after creation?
    // Caller usually creates object? No, typically factory method returns object.
    
    // We need to construct `com.dbcenter.VectorStore`.
    let cls = env.find_class("com/dbcenter/VectorStore").expect("Couldn't find VectorStore class");
    let ctor = env.get_method_id(&cls, "<init>", "(JI)V").expect("Couldn't find constructor");
    
    // We need backend store object.
    // Core returns VectorStore<'a>.
    let dim = dimension as usize;
    
    // We cheat lifetime again.
    let db_ref: &'static DBCenter = unsafe { std::mem::transmute(db.inner.as_ref()) };
    
    match db_ref.vector_store(&name_str, dim) {
        Ok(store) => {
             let container = Box::new(JavaVectorStore {
                store,
                _parent: db.inner.clone(),
                dimension: dim,
             });
             let ptr_val = Box::into_raw(container) as jlong;
             
             let java_obj = env.new_object("com/dbcenter/VectorStore", "(JI)V", &[JValue::Long(ptr_val), JValue::Int(dimension)]).unwrap();
             java_obj.into_raw()
        }
        Err(e) => {
            env.throw_new("java/lang/RuntimeException", e.to_string()).unwrap();
            JObject::null().into_raw()
        }
    }
}

struct JavaVectorStore {
    store: dbcenter_core::vector_store::VectorStore<'static>,
    _parent: Arc<DBCenter>,
    dimension: usize,
}

#[no_mangle]
pub extern "system" fn Java_com_dbcenter_VectorStore_nativeFree(
    mut env: JNIEnv,
    obj: JObject,
) {
    let ptr = get_handle(&mut env, &obj);
     if ptr != 0 {
        unsafe {
            let _ = Box::from_raw(ptr as *mut JavaVectorStore);
        }
        set_handle(&mut env, &obj, 0);
    }
}

#[no_mangle]
pub extern "system" fn Java_com_dbcenter_VectorStore_nativeAdd(
    mut env: JNIEnv,
    obj: JObject,
    ids: jni::objects::JObjectArray, // String[]
    embeddings: jni::objects::JFloatArray, // float[] flat
) {
    let ptr = get_handle(&mut env, &obj);
    if ptr == 0 {
         env.throw_new("java/lang/IllegalStateException", "Closed").unwrap();
        return;
    }
    let container = unsafe { &*(ptr as *mut JavaVectorStore) };
    
    // Process IDs
    let count = env.get_array_length(&ids).unwrap();
    let mut id_vec = Vec::with_capacity(count as usize);
    for i in 0..count {
        let s_obj = env.get_object_array_element(&ids, i).unwrap();
        let s: String = env.get_string(&jni::objects::JString::from(s_obj)).unwrap().into();
        id_vec.push(s);
    }
    
    // Process Embeddings
    let dim = container.dimension;
    let embed_len = env.get_array_length(&embeddings).unwrap() as usize;
    if embed_len != (count as usize) * dim {
         env.throw_new("java/lang/IllegalArgumentException", "Embeddings dimension mismatch").unwrap();
        return;
    }
    
    // Copy float array
    let mut flat_embeds = vec![0.0f32; embed_len];
    env.get_float_array_region(&embeddings, 0, &mut flat_embeds).unwrap();
    
    let mut embed_vec = Vec::with_capacity(count as usize);
    for i in 0..(count as usize) {
        let start = i * dim;
        let end = start + dim;
        embed_vec.push(flat_embeds[start..end].to_vec());
    }

    if let Err(e) = container.store.add(&id_vec, &embed_vec, None) {
        env.throw_new("java/lang/RuntimeException", e.to_string()).unwrap();
    }
}

#[no_mangle]
pub extern "system" fn Java_com_dbcenter_VectorStore_nativeQuery(
    mut env: JNIEnv,
    obj: JObject,
    query: jni::objects::JFloatArray,
    n_results: jlong,
) -> jobject { // Returns QueryResult[]
    let ptr = get_handle(&mut env, &obj);
    if ptr == 0 {
        env.throw_new("java/lang/IllegalStateException", "Closed").unwrap();
        return JObject::null().into_raw();
    }
    let container = unsafe { &*(ptr as *mut JavaVectorStore) };
    let dim = container.dimension;
    
    let q_len = env.get_array_length(&query).unwrap() as usize;
    if q_len != dim {
         env.throw_new("java/lang/IllegalArgumentException", "Query dimension mismatch").unwrap();
         return JObject::null().into_raw();
    }
    
    let mut q_vec = vec![0.0f32; dim];
    env.get_float_array_region(&query, 0, &mut q_vec).unwrap();
    
    match container.store.query(&[q_vec], n_results, None) {
        Ok(results) => {
            // Convert to QueryResult[]
            let res_cls = env.find_class("com/dbcenter/QueryResult").expect("Find QueryResult");
            let res_ctor = env.get_method_id(&res_cls, "<init>", "(Ljava/lang/String;DF)V").expect("Ctor");
            // Constructor: String id, double distance, float dummy? No, wait metadata?
            // Let's assume Java QueryResult(String id, double distance, String metadata)
            // (Ljava/lang/String;DLjava/lang/String;)V
            
            // I need to confirm the Java class signature I will write.
            // Let's assume (String, double, String).
            let res_ctor = env.get_method_id(&res_cls, "<init>", "(Ljava/lang/String;DLjava/lang/String;)V").expect("Ctor");

            let arr_cls = env.find_class("[Lcom/dbcenter/QueryResult;").expect("Array class");
            // To create object array:
            let initial_elem = JObject::null(); 
            let jarr = env.new_object_array(results.len() as i32, &res_cls, &initial_elem).unwrap();
            
            for (i, res) in results.iter().enumerate() {
                let id_str = res["id"].as_str().unwrap_or("");
                let j_id = env.new_string(id_str).unwrap();
                
                let dist = res["distance"].as_f64().unwrap_or(0.0);
                
                let meta = res["metadata"].to_string();
                let j_meta = env.new_string(meta).unwrap();
                
                let args = [
                    jni::sys::jvalue { l: j_id.as_raw() },
                    jni::sys::jvalue { d: dist },
                    jni::sys::jvalue { l: j_meta.as_raw() }
                ];
                let j_res = unsafe {
                    env.new_object_unchecked(&res_cls, res_ctor, &args).unwrap()
                };
                env.set_object_array_element(&jarr, i as i32, &j_res).unwrap();
            }
            
            jarr.into_raw()
        }
        Err(e) => {
             env.throw_new("java/lang/RuntimeException", e.to_string()).unwrap();
             JObject::null().into_raw()
        }
    }
}
