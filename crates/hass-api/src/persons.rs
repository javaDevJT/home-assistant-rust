//! Native person collection and caller-bound profile transport.

use crate::{AppState, extensions::Identity};
use hass_auth::AuthManager;
use hass_core::{Error, Result};
use hass_person::{FieldUpdate, PersonCreate, PersonManager, PersonUpdate};
use serde_json::{Map, Value, json};
use std::sync::OnceLock;

pub(crate) async fn handle(
    state: &AppState,
    identity: &Identity,
    command: &Value,
) -> Result<Value> {
    let manager = state
        .features
        .persons
        .clone()
        .ok_or_else(|| Error::NotFound("Person integration is not configured".into()))?;
    let auth = state.features.auth.clone();
    let user_id = identity.user_id.clone();
    let images = state.features.images.clone();
    let command = command.clone();
    tokio::task::spawn_blocking(move || handle_sync(manager, auth, user_id, images, &command))
        .await
        .map_err(|_| Error::Service("Person task failed".into()))?
}

fn handle_sync(
    manager: PersonManager,
    auth: Option<AuthManager>,
    user_id: Option<String>,
    images: Option<hass_image_upload::ImageManager>,
    command: &Value,
) -> Result<Value> {
    let object = command
        .as_object()
        .ok_or_else(|| Error::InvalidInput("Person command must be an object".into()))?;
    let kind = object.get("type").and_then(Value::as_str).unwrap_or("");
    let mut fields: Map<String, Value> = object
        .iter()
        .filter(|(key, _)| !matches!(key.as_str(), "id" | "type"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    match kind {
        "person/list" => {
            require_empty(&fields)?;
            let (snapshot, _) = manager.snapshot_and_subscribe()?;
            Ok(json!({"storage":snapshot.storage,"config":snapshot.config}))
        }
        "person/create" => {
            let create: PersonCreate = serde_json::from_value(Value::Object(fields))
                .map_err(|_| Error::InvalidInput("Invalid person creation fields".into()))?;
            encode(manager.create(create)?)
        }
        "person/update" => {
            let id = take_id(&mut fields)?;
            let update = parse_update(fields)?;
            encode(manager.update(&id, update)?)
        }
        "person/delete" => {
            let id = take_id(&mut fields)?;
            require_empty(&fields)?;
            manager.delete(&id)?;
            Ok(Value::Null)
        }
        "person/update_own_profile" => update_own_profile(&manager, auth, user_id, images, fields),
        _ => Err(Error::InvalidInput("Unsupported person command".into())),
    }
}

fn parse_update(mut fields: Map<String, Value>) -> Result<PersonUpdate> {
    let name = fields
        .remove("name")
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| Error::InvalidInput("Person name must be a string".into()))
        })
        .transpose()?;
    let user_id = fields
        .remove("user_id")
        .map(|value| nullable_string(value, "user_id").map(FieldUpdate::Set))
        .transpose()?
        .unwrap_or_default();
    let picture = fields
        .remove("picture")
        .map(|value| nullable_string(value, "picture").map(FieldUpdate::Set))
        .transpose()?
        .unwrap_or_default();
    let device_trackers = fields.remove("device_trackers").map(trackers).transpose()?;
    require_empty(&fields)?;
    Ok(PersonUpdate {
        name,
        user_id,
        picture,
        device_trackers,
    })
}

fn update_own_profile(
    manager: &PersonManager,
    auth: Option<AuthManager>,
    user_id: Option<String>,
    images: Option<hass_image_upload::ImageManager>,
    mut fields: Map<String, Value>,
) -> Result<Value> {
    let id = user_id.ok_or_else(|| {
        Error::InvalidInput("A user access token is required for profile operations".into())
    })?;
    let auth = auth.ok_or_else(|| Error::NotFound("Authentication is not configured".into()))?;
    let mut user = auth.get_user(&id)?;
    if !user.is_active {
        return Err(Error::InvalidInput(
            "Inactive users cannot change their profile".into(),
        ));
    }
    if user.system_generated {
        return Err(Error::InvalidInput(
            "System-generated users cannot change their profile".into(),
        ));
    }
    let name = fields
        .remove("name")
        .map(|value| {
            value
                .as_str()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| Error::InvalidInput("Profile name must be a nonempty string".into()))
        })
        .transpose()?;
    let picture = fields
        .remove("picture")
        .map(|value| nullable_string(value, "picture"))
        .transpose()?;
    require_empty(&fields)?;
    let linked = manager.person_for_user(&id)?;
    if let Some(picture) = &picture {
        match &linked {
            Some(person) if person.editable => {}
            Some(_) => {
                return Err(Error::InvalidInput(
                    "The linked person is not editable".into(),
                ));
            }
            None => {
                return Err(Error::InvalidInput(
                    "No person is linked to this user".into(),
                ));
            }
        }
        if let Some(picture) = picture {
            validate_uploaded_picture(images.as_ref(), picture)?;
        }
    }
    if let Some(name) = &name
        && name != &user.name
    {
        user = auth.update_own_name(&id, name)?;
    }
    let person = if let Some(linked) = linked.filter(|person| person.editable) {
        if name.is_some() || picture.is_some() {
            Some(manager.update(
                &linked.person.id,
                PersonUpdate {
                    name,
                    user_id: FieldUpdate::Keep,
                    picture: picture.map(FieldUpdate::Set).unwrap_or_default(),
                    device_trackers: Some(linked.person.device_trackers),
                },
            )?)
        } else {
            Some(linked.person)
        }
    } else {
        None
    };
    Ok(json!({"user_name":user.name,"person":person}))
}

fn validate_uploaded_picture(
    images: Option<&hass_image_upload::ImageManager>,
    picture: &str,
) -> Result<()> {
    static PICTURE: OnceLock<regex::Regex> = OnceLock::new();
    let pattern = PICTURE.get_or_init(|| {
        regex::Regex::new(r"^/api/image/serve/(?P<image_id>[0-9a-f]{32})/(original|\d+x\d+)$")
            .expect("constant person picture URL pattern")
    });
    let captures = pattern
        .captures(picture)
        .ok_or_else(|| Error::InvalidInput("Picture must reference an uploaded image".into()))?;
    let images = images
        .ok_or_else(|| Error::InvalidInput("Profile picture uploads are unavailable".into()))?;
    if !images.contains(&captures["image_id"])? {
        return Err(Error::InvalidInput(
            "Picture must reference an uploaded image".into(),
        ));
    }
    Ok(())
}

fn trackers(value: Value) -> Result<Vec<String>> {
    let values = match value {
        Value::Array(values) => values,
        other => vec![other],
    };
    values
        .into_iter()
        .filter(|value| match value {
            Value::Null => false,
            Value::Bool(value) => *value,
            Value::Number(value) => value.as_f64().is_none_or(|value| value != 0.0),
            Value::String(value) => !value.is_empty(),
            Value::Array(value) => !value.is_empty(),
            Value::Object(value) => !value.is_empty(),
        })
        .map(|value| match value {
            Value::String(value) => Ok(value),
            _ => Err(Error::InvalidInput(
                "device_trackers must contain entity IDs".into(),
            )),
        })
        .collect()
}

fn nullable_string(value: Value, field: &str) -> Result<Option<String>> {
    match value {
        Value::Null => Ok(None),
        Value::String(value) => Ok(Some(value)),
        _ => Err(Error::InvalidInput(format!(
            "{field} must be a string or null"
        ))),
    }
}

fn take_id(fields: &mut Map<String, Value>) -> Result<String> {
    fields
        .remove("person_id")
        .and_then(|value| value.as_str().map(str::to_owned))
        .filter(|id| !id.is_empty())
        .ok_or_else(|| Error::InvalidInput("person_id must be a nonempty string".into()))
}

fn require_empty(fields: &Map<String, Value>) -> Result<()> {
    if fields.is_empty() {
        Ok(())
    } else {
        Err(Error::InvalidInput(
            "Unexpected person command fields".into(),
        ))
    }
}

fn encode(value: impl serde::Serialize) -> Result<Value> {
    serde_json::to_value(value).map_err(|_| Error::Service("Cannot serialize person".into()))
}
