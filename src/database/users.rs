use crate::{utils, Error, Result};
use js_int::UInt;
use ruma::{
    api::client::r0::{
        device::Device,
        keys::{AlgorithmAndDeviceId, DeviceKeys, KeyAlgorithm, OneTimeKey},
    },
    events::{to_device::AnyToDeviceEvent, EventJson, EventType},
    identifiers::UserId,
};
use std::{collections::BTreeMap, convert::TryFrom, time::SystemTime};

pub struct Users {
    pub(super) userid_password: sled::Tree,
    pub(super) userid_displayname: sled::Tree,
    pub(super) userid_avatarurl: sled::Tree,
    pub(super) userdeviceid_token: sled::Tree,
    pub(super) userdeviceid_metadata: sled::Tree, // This is also used to check if a device exists
    pub(super) token_userdeviceid: sled::Tree,

    pub(super) onetimekeyid_onetimekeys: sled::Tree, // OneTimeKeyId = UserId + AlgorithmAndDeviceId
    pub(super) userdeviceid_devicekeys: sled::Tree,
    pub(super) devicekeychangeid_userid: sled::Tree, // DeviceKeyChangeId = Count

    pub(super) todeviceid_events: sled::Tree, // ToDeviceId = UserId + DeviceId + Count
}

impl Users {
    /// Check if a user has an account on this homeserver.
    pub fn exists(&self, user_id: &UserId) -> Result<bool> {
        Ok(self.userid_password.contains_key(user_id.to_string())?)
    }

    /// Create a new user account on this homeserver.
    pub fn create(&self, user_id: &UserId, hash: &str) -> Result<()> {
        self.userid_password.insert(user_id.to_string(), hash)?;
        Ok(())
    }

    /// Find out which user an access token belongs to.
    pub fn find_from_token(&self, token: &str) -> Result<Option<(UserId, String)>> {
        self.token_userdeviceid
            .get(token)?
            .map_or(Ok(None), |bytes| {
                let mut parts = bytes.split(|&b| b == 0xff);
                let user_bytes = parts.next().ok_or_else(|| {
                    Error::bad_database("User ID in token_userdeviceid is invalid.")
                })?;
                let device_bytes = parts.next().ok_or_else(|| {
                    Error::bad_database("Device ID in token_userdeviceid is invalid.")
                })?;

                Ok(Some((
                    UserId::try_from(utils::string_from_bytes(&user_bytes).map_err(|_| {
                        Error::bad_database("User ID in token_userdeviceid is invalid unicode.")
                    })?)
                    .map_err(|_| {
                        Error::bad_database("User ID in token_userdeviceid is invalid.")
                    })?,
                    utils::string_from_bytes(&device_bytes).map_err(|_| {
                        Error::bad_database("Device ID in token_userdeviceid is invalid.")
                    })?,
                )))
            })
    }

    /// Returns an iterator over all users on this homeserver.
    pub fn iter(&self) -> impl Iterator<Item = Result<UserId>> {
        self.userid_password.iter().keys().map(|bytes| {
            Ok(
                UserId::try_from(utils::string_from_bytes(&bytes?).map_err(|_| {
                    Error::bad_database("User ID in userid_password is invalid unicode.")
                })?)
                .map_err(|_| Error::bad_database("User ID in userid_password is invalid."))?,
            )
        })
    }

    /// Returns the password hash for the given user.
    pub fn password_hash(&self, user_id: &UserId) -> Result<Option<String>> {
        self.userid_password
            .get(user_id.to_string())?
            .map_or(Ok(None), |bytes| {
                Ok(Some(utils::string_from_bytes(&bytes).map_err(|_| {
                    Error::bad_database("Password hash in db is not valid string.")
                })?))
            })
    }

    /// Returns the displayname of a user on this homeserver.
    pub fn displayname(&self, user_id: &UserId) -> Result<Option<String>> {
        self.userid_displayname
            .get(user_id.to_string())?
            .map_or(Ok(None), |bytes| {
                Ok(Some(utils::string_from_bytes(&bytes).map_err(|_| {
                    Error::bad_database("Displayname in db is invalid.")
                })?))
            })
    }

    /// Sets a new displayname or removes it if displayname is None. You still need to nofify all rooms of this change.
    pub fn set_displayname(&self, user_id: &UserId, displayname: Option<String>) -> Result<()> {
        if let Some(displayname) = displayname {
            self.userid_displayname
                .insert(user_id.to_string(), &*displayname)?;
        } else {
            self.userid_displayname.remove(user_id.to_string())?;
        }

        Ok(())
    }

    /// Get a the avatar_url of a user.
    pub fn avatar_url(&self, user_id: &UserId) -> Result<Option<String>> {
        self.userid_avatarurl
            .get(user_id.to_string())?
            .map_or(Ok(None), |bytes| {
                Ok(Some(utils::string_from_bytes(&bytes).map_err(|_| {
                    Error::bad_database("Avatar URL in db is invalid.")
                })?))
            })
    }

    /// Sets a new avatar_url or removes it if avatar_url is None.
    pub fn set_avatar_url(&self, user_id: &UserId, avatar_url: Option<String>) -> Result<()> {
        if let Some(avatar_url) = avatar_url {
            self.userid_avatarurl
                .insert(user_id.to_string(), &*avatar_url)?;
        } else {
            self.userid_avatarurl.remove(user_id.to_string())?;
        }

        Ok(())
    }

    /// Adds a new device to a user.
    pub fn create_device(
        &self,
        user_id: &UserId,
        device_id: &str,
        token: &str,
        initial_device_display_name: Option<String>,
    ) -> Result<()> {
        // This method should never be called for nonexistent users.
        assert!(self.exists(user_id)?);

        let mut userdeviceid = user_id.to_string().as_bytes().to_vec();
        userdeviceid.push(0xff);
        userdeviceid.extend_from_slice(device_id.as_bytes());

        self.userdeviceid_metadata.insert(
            userdeviceid,
            serde_json::to_string(&Device {
                device_id: device_id.to_owned(),
                display_name: initial_device_display_name,
                last_seen_ip: None, // TODO
                last_seen_ts: Some(SystemTime::now()),
            })
            .expect("Device::to_string never fails.")
            .as_bytes(),
        )?;

        self.set_token(user_id, device_id, token)?;

        Ok(())
    }

    /// Removes a device from a user.
    pub fn remove_device(&self, user_id: &UserId, device_id: &str) -> Result<()> {
        let mut userdeviceid = user_id.to_string().as_bytes().to_vec();
        userdeviceid.push(0xff);
        userdeviceid.extend_from_slice(device_id.as_bytes());

        // Remove device keys
        self.userdeviceid_devicekeys.remove(&userdeviceid)?;

        // Remove tokens
        if let Some(old_token) = self.userdeviceid_token.remove(&userdeviceid)? {
            self.token_userdeviceid.remove(&old_token)?;
        }

        // Remove todevice events
        let mut prefix = userdeviceid.clone();
        prefix.push(0xff);

        for key in self.todeviceid_events.scan_prefix(&prefix).keys() {
            self.todeviceid_events.remove(key?)?;
        }

        // TODO: Remove onetimekeys

        self.userdeviceid_metadata.remove(&userdeviceid)?;

        Ok(())
    }

    /// Returns an iterator over all device ids of this user.
    pub fn all_device_ids(&self, user_id: &UserId) -> impl Iterator<Item = Result<String>> {
        let mut prefix = user_id.to_string().as_bytes().to_vec();
        prefix.push(0xff);
        // All devices have metadata
        self.userdeviceid_metadata
            .scan_prefix(prefix)
            .keys()
            .map(|bytes| {
                Ok(utils::string_from_bytes(
                    &*bytes?
                        .rsplit(|&b| b == 0xff)
                        .next()
                        .ok_or_else(|| Error::bad_database("UserDevice ID in db is invalid."))?,
                )
                .map_err(|_| {
                    Error::bad_database("Device ID in userdeviceid_metadata is invalid.")
                })?)
            })
    }

    /// Replaces the access token of one device.
    fn set_token(&self, user_id: &UserId, device_id: &str, token: &str) -> Result<()> {
        let mut userdeviceid = user_id.to_string().as_bytes().to_vec();
        userdeviceid.push(0xff);
        userdeviceid.extend_from_slice(device_id.as_bytes());

        // All devices have metadata
        assert!(self.userdeviceid_metadata.get(&userdeviceid)?.is_some());

        // Remove old token
        if let Some(old_token) = self.userdeviceid_token.get(&userdeviceid)? {
            self.token_userdeviceid.remove(old_token)?;
            // It will be removed from userdeviceid_token by the insert later
        }

        // Assign token to user device combination
        self.userdeviceid_token.insert(&userdeviceid, &*token)?;
        self.token_userdeviceid.insert(token, userdeviceid)?;

        Ok(())
    }

    pub fn add_one_time_key(
        &self,
        user_id: &UserId,
        device_id: &str,
        one_time_key_key: &AlgorithmAndDeviceId,
        one_time_key_value: &OneTimeKey,
    ) -> Result<()> {
        let mut key = user_id.to_string().as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(device_id.as_bytes());

        // All devices have metadata
        // Only existing devices should be able to call this.
        assert!(self.userdeviceid_metadata.get(&key)?.is_some());

        key.push(0xff);
        // TODO: Use AlgorithmAndDeviceId::to_string when it's available (and update everything,
        // because there are no wrapping quotation marks anymore)
        key.extend_from_slice(
            &serde_json::to_string(one_time_key_key)
                .expect("AlgorithmAndDeviceId::to_string always works")
                .as_bytes(),
        );

        self.onetimekeyid_onetimekeys.insert(
            &key,
            &*serde_json::to_string(&one_time_key_value)
                .expect("OneTimeKey::to_string always works"),
        )?;

        Ok(())
    }

    pub fn take_one_time_key(
        &self,
        user_id: &UserId,
        device_id: &str,
        key_algorithm: &KeyAlgorithm,
    ) -> Result<Option<(AlgorithmAndDeviceId, OneTimeKey)>> {
        let mut prefix = user_id.to_string().as_bytes().to_vec();
        prefix.push(0xff);
        prefix.extend_from_slice(device_id.as_bytes());
        prefix.push(0xff);
        prefix.push(b'"'); // Annoying quotation mark
        prefix.extend_from_slice(key_algorithm.to_string().as_bytes());
        prefix.push(b':');

        self.onetimekeyid_onetimekeys
            .scan_prefix(&prefix)
            .next()
            .map(|r| {
                let (key, value) = r?;
                self.onetimekeyid_onetimekeys.remove(&key)?;

                Ok((
                    serde_json::from_slice(
                        &*key
                            .rsplit(|&b| b == 0xff)
                            .next()
                            .ok_or_else(|| Error::bad_database("OneTimeKeyId in db is invalid."))?,
                    )
                    .map_err(|_| Error::bad_database("OneTimeKeyId in db is invalid."))?,
                    serde_json::from_slice(&*value)
                        .map_err(|_| Error::bad_database("OneTimeKeys in db are invalid."))?,
                ))
            })
            .transpose()
    }

    pub fn count_one_time_keys(
        &self,
        user_id: &UserId,
        device_id: &str,
    ) -> Result<BTreeMap<KeyAlgorithm, UInt>> {
        let mut userdeviceid = user_id.to_string().as_bytes().to_vec();
        userdeviceid.push(0xff);
        userdeviceid.extend_from_slice(device_id.as_bytes());

        let mut counts = BTreeMap::new();

        for algorithm in self
            .onetimekeyid_onetimekeys
            .scan_prefix(&userdeviceid)
            .keys()
            .map(|bytes| {
                Ok::<_, Error>(
                    serde_json::from_slice::<AlgorithmAndDeviceId>(
                        &*bytes?.rsplit(|&b| b == 0xff).next().ok_or_else(|| {
                            Error::bad_database("OneTimeKey ID in db is invalid.")
                        })?,
                    )
                    .map_err(|_| Error::bad_database("AlgorithmAndDeviceID in db is invalid."))?
                    .0,
                )
            })
        {
            *counts.entry(algorithm?).or_default() += UInt::from(1_u32);
        }

        Ok(counts)
    }

    pub fn add_device_keys(
        &self,
        user_id: &UserId,
        device_id: &str,
        device_keys: &DeviceKeys,
        globals: &super::globals::Globals,
    ) -> Result<()> {
        let mut userdeviceid = user_id.to_string().as_bytes().to_vec();
        userdeviceid.push(0xff);
        userdeviceid.extend_from_slice(device_id.as_bytes());

        self.userdeviceid_devicekeys.insert(
            &userdeviceid,
            &*serde_json::to_string(&device_keys).expect("DeviceKeys::to_string always works"),
        )?;

        self.devicekeychangeid_userid
            .insert(globals.next_count()?.to_be_bytes(), &*user_id.to_string())?;

        Ok(())
    }

    pub fn get_device_keys(
        &self,
        user_id: &UserId,
        device_id: &str,
    ) -> impl Iterator<Item = Result<DeviceKeys>> {
        let mut key = user_id.to_string().as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(device_id.as_bytes());

        self.userdeviceid_devicekeys
            .scan_prefix(key)
            .values()
            .map(|bytes| {
                Ok(serde_json::from_slice(&bytes?)
                    .map_err(|_| Error::bad_database("DeviceKeys in db are invalid."))?)
            })
    }

    pub fn device_keys_changed(&self, since: u64) -> impl Iterator<Item = Result<UserId>> {
        self.devicekeychangeid_userid
            .range(since.to_be_bytes()..)
            .values()
            .map(|bytes| {
                Ok(
                    UserId::try_from(utils::string_from_bytes(&bytes?).map_err(|_| {
                        Error::bad_database(
                            "User ID in devicekeychangeid_userid is invalid unicode.",
                        )
                    })?)
                    .map_err(|_| {
                        Error::bad_database("User ID in devicekeychangeid_userid is invalid.")
                    })?,
                )
            })
    }

    pub fn all_device_keys(
        &self,
        user_id: &UserId,
    ) -> impl Iterator<Item = Result<(String, DeviceKeys)>> {
        let mut key = user_id.to_string().as_bytes().to_vec();
        key.push(0xff);

        self.userdeviceid_devicekeys.scan_prefix(key).map(|r| {
            let (key, value) = r?;
            let userdeviceid = utils::string_from_bytes(
                key.rsplit(|&b| b == 0xff)
                    .next()
                    .ok_or_else(|| Error::bad_database("UserDeviceID in db is invalid."))?,
            )
            .map_err(|_| Error::bad_database("UserDeviceId in db is invalid."))?;
            Ok((
                userdeviceid,
                serde_json::from_slice(&*value)
                    .map_err(|_| Error::bad_database("DeviceKeys in db are invalid."))?,
            ))
        })
    }

    pub fn add_to_device_event(
        &self,
        sender: &UserId,
        target_user_id: &UserId,
        target_device_id: &str,
        event_type: &EventType,
        content: serde_json::Value,
        globals: &super::globals::Globals,
    ) -> Result<()> {
        let mut key = target_user_id.to_string().as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(target_device_id.as_bytes());
        key.push(0xff);
        key.extend_from_slice(&globals.next_count()?.to_be_bytes());

        let mut json = serde_json::Map::new();
        json.insert("type".to_owned(), event_type.to_string().into());
        json.insert("sender".to_owned(), sender.to_string().into());
        json.insert("content".to_owned(), content);

        self.todeviceid_events.insert(
            &key,
            &*serde_json::to_string(&json).expect("Map::to_string always works"),
        )?;

        Ok(())
    }

    pub fn take_to_device_events(
        &self,
        user_id: &UserId,
        device_id: &str,
        max: usize,
    ) -> Result<Vec<EventJson<AnyToDeviceEvent>>> {
        let mut events = Vec::new();

        let mut prefix = user_id.to_string().as_bytes().to_vec();
        prefix.push(0xff);
        prefix.extend_from_slice(device_id.as_bytes());
        prefix.push(0xff);

        for result in self.todeviceid_events.scan_prefix(&prefix).take(max) {
            let (key, value) = result?;
            events.push(
                serde_json::from_slice(&*value)
                    .map_err(|_| Error::bad_database("Event in todeviceid_events is invalid."))?,
            );
            self.todeviceid_events.remove(key)?;
        }

        Ok(events)
    }

    pub fn update_device_metadata(
        &self,
        user_id: &UserId,
        device_id: &str,
        device: &Device,
    ) -> Result<()> {
        let mut userdeviceid = user_id.to_string().as_bytes().to_vec();
        userdeviceid.push(0xff);
        userdeviceid.extend_from_slice(device_id.as_bytes());

        // Only existing devices should be able to call this.
        assert!(self.userdeviceid_metadata.get(&userdeviceid)?.is_some());

        self.userdeviceid_metadata.insert(
            userdeviceid,
            serde_json::to_string(device)
                .expect("Device::to_string always works")
                .as_bytes(),
        )?;

        Ok(())
    }

    /// Get device metadata.
    pub fn get_device_metadata(&self, user_id: &UserId, device_id: &str) -> Result<Option<Device>> {
        let mut userdeviceid = user_id.to_string().as_bytes().to_vec();
        userdeviceid.push(0xff);
        userdeviceid.extend_from_slice(device_id.as_bytes());

        self.userdeviceid_metadata
            .get(&userdeviceid)?
            .map_or(Ok(None), |bytes| {
                Ok(Some(serde_json::from_slice(&bytes).map_err(|_| {
                    Error::bad_database("Metadata in userdeviceid_metadata is invalid.")
                })?))
            })
    }

    pub fn all_devices_metadata(&self, user_id: &UserId) -> impl Iterator<Item = Result<Device>> {
        let mut key = user_id.to_string().as_bytes().to_vec();
        key.push(0xff);

        self.userdeviceid_metadata
            .scan_prefix(key)
            .values()
            .map(|bytes| {
                Ok(serde_json::from_slice::<Device>(&bytes?).map_err(|_| {
                    Error::bad_database("Device in userdeviceid_metadata is invalid.")
                })?)
            })
    }
}
