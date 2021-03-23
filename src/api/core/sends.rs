use std::path::Path;

use chrono::{DateTime, Duration, Utc};
use rocket::{
    data::TempFile,
    form::{Form, FromForm},
};
use rocket_contrib::json::Json;
use serde_json::Value;

use crate::{
    api::{ApiResult, EmptyResult, JsonResult, JsonUpcase, Notify, UpdateType},
    auth::{Headers, Host},
    db::{models::*, DbConn},
    CONFIG,
};

pub fn routes() -> Vec<rocket::Route> {
    routes![
        post_send,
        post_send_file,
        post_access,
        post_access_file,
        put_send,
        delete_send,
        put_remove_password
    ]
}

pub fn start_send_deletion_scheduler(pool: crate::db::DbPool) {
    std::thread::spawn(move || {
        loop {
            if let Ok(conn) = pool.get() {
                info!("Initiating send deletion");
                for send in Send::find_all(&conn) {
                    if chrono::Utc::now().naive_utc() >= send.deletion_date {
                        send.delete(&conn).ok();
                    }
                }
            }

            std::thread::sleep(std::time::Duration::from_secs(3600));
        }
    });
}

#[derive(Deserialize)]
#[allow(non_snake_case)]
pub struct SendData {
    pub Type: i32,
    pub Key: String,
    pub Password: Option<String>,
    pub MaxAccessCount: Option<i32>,
    pub ExpirationDate: Option<DateTime<Utc>>,
    pub DeletionDate: DateTime<Utc>,
    pub Disabled: bool,

    // Data field
    pub Name: String,
    pub Notes: Option<String>,
    pub Text: Option<Value>,
    pub File: Option<Value>,
}

/// Enforces the `Disable Send` policy. A non-owner/admin user belonging to
/// an org with this policy enabled isn't allowed to create new Sends or
/// modify existing ones, but is allowed to delete them.
///
/// Ref: https://bitwarden.com/help/article/policies/#disable-send
fn enforce_disable_send_policy(headers: &Headers, conn: &DbConn) -> EmptyResult {
    let user_uuid = &headers.user.uuid;
    let policy_type = OrgPolicyType::DisableSend;
    if OrgPolicy::is_applicable_to_user(user_uuid, policy_type, conn) {
        err!("Due to an Enterprise Policy, you are only able to delete an existing Send.")
    }
    Ok(())
}

fn create_send(data: SendData, user_uuid: String) -> ApiResult<Send> {
    let data_val = if data.Type == SendType::Text as i32 {
        data.Text
    } else if data.Type == SendType::File as i32 {
        data.File
    } else {
        err!("Invalid Send type")
    };

    let data_str = if let Some(mut d) = data_val {
        d.as_object_mut().and_then(|o| o.remove("Response"));
        serde_json::to_string(&d)?
    } else {
        err!("Send data not provided");
    };

    if data.DeletionDate > Utc::now() + Duration::days(31) {
        err!(
            "You cannot have a Send with a deletion date that far into the future. Adjust the Deletion Date to a value less than 31 days from now and try again."
        );
    }

    let mut send = Send::new(data.Type, data.Name, data_str, data.Key, data.DeletionDate.naive_utc());
    send.user_uuid = Some(user_uuid);
    send.notes = data.Notes;
    send.max_access_count = data.MaxAccessCount;
    send.expiration_date = data.ExpirationDate.map(|d| d.naive_utc());
    send.disabled = data.Disabled;
    send.atype = data.Type;

    send.set_password(data.Password.as_deref());

    Ok(send)
}

#[post("/sends", data = "<data>")]
fn post_send(data: JsonUpcase<SendData>, headers: Headers, conn: DbConn, nt: Notify) -> JsonResult {
    enforce_disable_send_policy(&headers, &conn)?;

    let data: SendData = data.into_inner().data;

    if data.Type == SendType::File as i32 {
        err!("File sends should use /api/sends/file")
    }

    let mut send = create_send(data, headers.user.uuid.clone())?;
    send.save(&conn)?;
    nt.send_user_update(UpdateType::SyncSendCreate, &headers.user);

    Ok(Json(send.to_json()))
}

#[derive(FromForm)]
struct UploadData<'f> {
    model: JsonUpcase<SendData>,
    data: TempFile<'f>,
}

#[post("/sends/file", format = "multipart/form-data", data = "<data>")]
async fn post_send_file(data: Form<UploadData<'_>>, headers: Headers, conn: DbConn, nt: Notify<'_>) -> JsonResult {
    enforce_disable_send_policy(&headers, &conn)?;

    let mut data = data.into_inner();

    // Create the Send
    let mut send = create_send(data.model.into_inner().data, headers.user.uuid.clone())?;
    let file_id: String = data_encoding::HEXLOWER.encode(&crate::crypto::get_random(vec![0; 32]));

    if send.atype != SendType::File as i32 {
        err!("Send content is not a file");
    }

    let file_path = Path::new(&CONFIG.sends_folder()).join(&send.uuid).join(&file_id);

    // Get the file length and add an extra 10% to avoid issues
    const SIZE_110_MB: u64 = 115_343_360;

    let size_limit = match CONFIG.user_attachment_limit() {
        Some(0) => err!("File uploads are disabled"),
        Some(limit_kb) => {
            let left = (limit_kb * 1024) - Attachment::size_by_user(&headers.user.uuid, &conn);
            if left <= 0 {
                err!("Attachment size limit reached! Delete some files to open space")
            }
            std::cmp::Ord::max(left as u64, SIZE_110_MB)
        }
        None => SIZE_110_MB,
    };

    // Check the size limits
    let size = data.data.len();
    if size > size_limit {
        err!(format!(
            "Send size limit exceeded. Free: {}, used: {}",
            size_limit, size
        ))
    }

    tokio::fs::create_dir_all(file_path.parent().expect("Path has no parent")).await?;
    data.data.persist_to(file_path).await?;

    // Set ID and sizes
    let mut data_value: Value = serde_json::from_str(&send.data)?;
    if let Some(o) = data_value.as_object_mut() {
        o.insert(String::from("Id"), Value::String(file_id));
        o.insert(String::from("Size"), Value::Number(size.into()));
        o.insert(
            String::from("SizeName"),
            Value::String(crate::util::get_display_size(size as i32)),
        );
    }
    send.data = serde_json::to_string(&data_value)?;

    // Save the changes in the database
    send.save(&conn)?;
    nt.send_user_update(UpdateType::SyncSendCreate, &headers.user);

    Ok(Json(send.to_json()))
}

#[derive(Deserialize)]
#[allow(non_snake_case)]
pub struct SendAccessData {
    pub Password: Option<String>,
}

#[post("/sends/access/<access_id>", data = "<data>")]
fn post_access(access_id: String, data: JsonUpcase<SendAccessData>, conn: DbConn) -> JsonResult {
    let mut send = match Send::find_by_access_id(&access_id, &conn) {
        Some(s) => s,
        None => err_code!("Send not found", 404),
    };

    if let Some(max_access_count) = send.max_access_count {
        if send.access_count >= max_access_count {
            err_code!("Max access count reached", 404);
        }
    }

    if let Some(expiration) = send.expiration_date {
        if Utc::now().naive_utc() >= expiration {
            err_code!("Send has expired", 404)
        }
    }

    if Utc::now().naive_utc() >= send.deletion_date {
        err_code!("Send has been deleted", 404)
    }

    if send.disabled {
        err_code!("Send has been disabled", 404)
    }

    if send.password_hash.is_some() {
        match data.into_inner().data.Password {
            Some(ref p) if send.check_password(p) => { /* Nothing to do here */ }
            Some(_) => err!("Invalid password."),
            None => err_code!("Password not provided", 401),
        }
    }

    // Files are incremented during the download
    if send.atype == SendType::Text as i32 {
        send.access_count += 1;
    }

    send.save(&conn)?;

    Ok(Json(send.to_json_access()))
}

#[post("/sends/<send_id>/access/file/<file_id>", data = "<data>")]
fn post_access_file(
    send_id: String,
    file_id: String,
    data: JsonUpcase<SendAccessData>,
    host: Host,
    conn: DbConn,
) -> JsonResult {
    let mut send = match Send::find_by_uuid(&send_id, &conn) {
        Some(s) => s,
        None => err_code!("Send not found", 404),
    };

    if let Some(max_access_count) = send.max_access_count {
        if send.access_count >= max_access_count {
            err_code!("Max access count reached", 404);
        }
    }

    if let Some(expiration) = send.expiration_date {
        if Utc::now().naive_utc() >= expiration {
            err_code!("Send has expired", 404)
        }
    }

    if Utc::now().naive_utc() >= send.deletion_date {
        err_code!("Send has been deleted", 404)
    }

    if send.disabled {
        err_code!("Send has been disabled", 404)
    }

    if send.password_hash.is_some() {
        match data.into_inner().data.Password {
            Some(ref p) if send.check_password(p) => { /* Nothing to do here */ }
            Some(_) => err!("Invalid password."),
            None => err_code!("Password not provided", 401),
        }
    }

    send.access_count += 1;

    send.save(&conn)?;

    Ok(Json(json!({
        "Object": "send-fileDownload",
        "Id": file_id,
        "Url": format!("{}/sends/{}/{}", &host.host, send_id, file_id)
    })))
}

#[put("/sends/<id>", data = "<data>")]
fn put_send(id: String, data: JsonUpcase<SendData>, headers: Headers, conn: DbConn, nt: Notify) -> JsonResult {
    enforce_disable_send_policy(&headers, &conn)?;

    let data: SendData = data.into_inner().data;

    let mut send = match Send::find_by_uuid(&id, &conn) {
        Some(s) => s,
        None => err!("Send not found"),
    };

    if send.user_uuid.as_ref() != Some(&headers.user.uuid) {
        err!("Send is not owned by user")
    }

    if send.atype != data.Type {
        err!("Sends can't change type")
    }

    // When updating a file Send, we receive nulls in the File field, as it's immutable,
    // so we only need to update the data field in the Text case
    if data.Type == SendType::Text as i32 {
        let data_str = if let Some(mut d) = data.Text {
            d.as_object_mut().and_then(|d| d.remove("Response"));
            serde_json::to_string(&d)?
        } else {
            err!("Send data not provided");
        };
        send.data = data_str;
    }

    if data.DeletionDate > Utc::now() + Duration::days(31) {
        err!(
            "You cannot have a Send with a deletion date that far into the future. Adjust the Deletion Date to a value less than 31 days from now and try again."
        );
    }
    send.name = data.Name;
    send.akey = data.Key;
    send.deletion_date = data.DeletionDate.naive_utc();
    send.notes = data.Notes;
    send.max_access_count = data.MaxAccessCount;
    send.expiration_date = data.ExpirationDate.map(|d| d.naive_utc());
    send.disabled = data.Disabled;

    // Only change the value if it's present
    if let Some(password) = data.Password {
        send.set_password(Some(&password));
    }

    send.save(&conn)?;
    nt.send_user_update(UpdateType::SyncSendUpdate, &headers.user);

    Ok(Json(send.to_json()))
}

#[delete("/sends/<id>")]
fn delete_send(id: String, headers: Headers, conn: DbConn, nt: Notify) -> EmptyResult {
    let send = match Send::find_by_uuid(&id, &conn) {
        Some(s) => s,
        None => err!("Send not found"),
    };

    if send.user_uuid.as_ref() != Some(&headers.user.uuid) {
        err!("Send is not owned by user")
    }

    send.delete(&conn)?;
    nt.send_user_update(UpdateType::SyncSendDelete, &headers.user);

    Ok(())
}

#[put("/sends/<id>/remove-password")]
fn put_remove_password(id: String, headers: Headers, conn: DbConn, nt: Notify) -> JsonResult {
    enforce_disable_send_policy(&headers, &conn)?;

    let mut send = match Send::find_by_uuid(&id, &conn) {
        Some(s) => s,
        None => err!("Send not found"),
    };

    if send.user_uuid.as_ref() != Some(&headers.user.uuid) {
        err!("Send is not owned by user")
    }

    send.set_password(None);
    send.save(&conn)?;
    nt.send_user_update(UpdateType::SyncSendUpdate, &headers.user);

    Ok(Json(send.to_json()))
}
