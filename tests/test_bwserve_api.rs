use bwrap::bwserve_api::*;
use rstest::rstest;

use anyhow::{Result, bail};
use httpmock::prelude::*;
use sonic_rs::json;

#[tokio::test]
#[rstest]
#[case("github.com")]
async fn bw_get_item_test(#[case] id: &str) -> Result<()> {
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method(GET).path(format!("/object/item/{}", id));
            then.status(200).body(
                json!({
                    "success": true,
                    "data": {
                        "type": 1,
                        "name": "github.com",
                        "favorite": false,
                        "reprompt": 0,
                        "id": "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx",
                        "collectionIds": [],
                        "object": "item",
                        "folderId": "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx",
                        "fields": [
                        {
                            "type": 0,
                            "name": "PAT_GITEA",
                            "value": "xxx_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
                        },
                        {
                            "type": 0,
                            "name": "PAT_RSSHUB",
                            "value": "xxx_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
                        },
                        ],
                        "login": {
                            "uris": [{ "uri": "https://github.com" }],
                            "fido2Credentials": [],
                            "username": "xxxxxxxx@xxxxx.xxx",
                            "password": "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
                            "totp": "xxxxxxxxxxxxxxxx",
                            "passwordRevisionDate": null
                        },
                        "passwordHistory": [],
                        "creationDate": "2023-11-09T17:31:37.377Z",
                        "revisionDate": "2026-05-02T03:11:42.698Z",
                        "attachments": []
                    }
                })
                .to_string(),
            );
        })
        .await;
    let api = BWServeApi::new(&server.url("/"))?;
    let resp_data = api
        .get(&BWGetArgs {
            object: "item".to_string(),
            id: id.to_string(),
        })
        .await?;
    assert!(resp_data.success);
    let BWServeGetRespData::Item(v) = resp_data.data.as_ref().unwrap() else {
        bail!("not item type {:?}", resp_data)
    };
    assert_eq!(v.name, id);
    Ok(())
}
