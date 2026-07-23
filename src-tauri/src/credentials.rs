use keyring::Entry;

const CREDENTIAL_SERVICE: &str = "com.imageannotation.desktop";

pub fn store_access_token(project_id: &str, access_token: &str) -> Result<(), String> {
    if project_id.trim().is_empty() {
        return Err("project ID is required".to_string());
    }
    if access_token.trim().is_empty() {
        return Err("access token is required".to_string());
    }
    credential_entry(project_id)?
        .set_password(access_token)
        .map_err(|err| format!("failed to store project credential: {err}"))
}

pub fn read_access_token(project_id: &str) -> Result<Option<String>, String> {
    match credential_entry(project_id)?.get_password() {
        Ok(token) => Ok(Some(token)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(format!("failed to read project credential: {error}")),
    }
}

pub fn delete_access_token(project_id: &str) -> Result<(), String> {
    match credential_entry(project_id)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(format!("failed to delete project credential: {error}")),
    }
}

fn credential_entry(project_id: &str) -> Result<Entry, String> {
    Entry::new(CREDENTIAL_SERVICE, &format!("project:{project_id}"))
        .map_err(|err| format!("failed to open system credential store: {err}"))
}
