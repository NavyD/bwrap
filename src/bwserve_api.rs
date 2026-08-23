use anyhow::{Result, bail};
use heck::ToKebabCase;
use reqwest::Client;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sonic_rs::{
    JsonContainerTrait, JsonValueTrait, ValueRef,
    from_slice as de_json_from_slice,
};
#[allow(unused_imports)]
use tracing::{debug, error, info, instrument, trace, warn};
use url::Url;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VaultItemField {
    #[serde(rename = "type")]
    pub field_type: i32,
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultItemLoginUri {
    pub uri: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VaultItemLogin {
    pub uris: Vec<VaultItemLoginUri>,
    // 原 "fido2Credentials" 映射为 fido2_credentials
    // pub fido2_credentials: Vec<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub totp: Option<String>,
    // 可选字符串，对应 Python 中的 str | None
    pub password_revision_date: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VaultItem {
    // 原 "type" 字段重命名
    #[serde(rename = "type")]
    pub item_type: i32,
    pub name: String,
    pub favorite: bool,
    // pub reprompt: i32,
    pub id: String,
    pub collection_ids: Vec<String>,
    // 原 "object" 字段，类型改为 VaultItemObjectKind
    pub object: String,
    // pub object: VaultItemObjectKind,
    pub folder_id: Option<String>,
    pub fields: Vec<VaultItemField>,
    pub login: Option<VaultItemLogin>,
    // passwordHistory 和 attachments 在 Python 中为 list[Any]，这里用 Vec<Value> 表示任意 JSON 数组
    // pub password_history: Vec<Value>,
    pub creation_date: String,
    pub revision_date: String,
    // pub attachments: Vec<Value>,
}

// BWServeResult 的 data 字段可能是多种类型，使用 untagged 枚举处理
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum BWServeGetRespData {
    Item(Box<VaultItem>), // 单个 VaultItem
    /// 表示 vault item 的核心属性如 username, password, totp...
    ItemProp {
        object: String,
        data: String,
    },
    // 找到多个
    MultiItemsFailure(Vec<String>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VaultCollection {
    pub id: String,
    pub organization_id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VaultFolder {
    pub name: String,
    pub object: String,
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum BWServeListRespData {
    Items(Vec<VaultItem>),
    Collections(Vec<VaultCollection>),
    Folders(Vec<VaultFolder>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BWServeRespObjectData<T> {
    pub object: String,
    pub data: T,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BWServeResp<T> {
    pub success: bool,
    pub message: Option<String>,
    // data 可为 null，故使用 Option
    pub data: Option<T>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BWServeStatusTemplate {
    pub server_url: String,
    pub last_sync: String,
    pub user_email: String,
    pub user_id: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BWServeStatusRespData {
    pub object: String,
    pub template: BWServeStatusTemplate,
}

#[derive(clap::Args, Debug, Clone, Serialize)]
pub struct BwListArgs {
    // url 路径如 /list/object/$object, list/object/items, list/object/folders
    #[serde(skip_serializing)]
    pub object: String,

    // 下面所有的均为搜索参数如： ?search=xx.x&trash=true&...
    // NOTE: bool 类型只有当 true 时才能出现在 url 参数中，否则会被其处理为 true
    // 即使设置为 `?search=xx.x&trash=false`
    #[arg(long)]
    pub search: Option<String>,
    #[arg(long)]
    pub url: Option<String>,
    #[arg(long)]
    pub folderid: Option<String>,
    #[arg(long)]
    pub collectionid: Option<String>,
    #[arg(long)]
    pub organizationid: Option<String>,
    #[arg(long)]
    pub trash: bool,
    #[arg(long)]
    pub archived: bool,
}

#[derive(clap::Args, Debug, Clone)]
pub struct BWGetArgs {
    pub object: String,
    pub id: String,
}

#[derive(Debug, Clone)]
pub struct BWServeApi {
    base_url: Url,
    client: reqwest::Client,
}

impl BWServeApi {
    pub fn new(base_url: &str) -> Result<Self> {
        let api_url = base_url.parse::<Url>()?;
        let (api_url, client_build) = match api_url.scheme() {
            // https://github.com/bitwarden/clients/pull/14262
            // [PM-20220] feat: Add support for fd and unix socket bindings
            // NOTE: 暂未支持 fd+: UnixSocketProvider from raw fd #2812
            // https://github.com/seanmonstar/reqwest/issues/2812
            #[cfg(unix)]
            "unix" => {
                let mut client_build = Client::builder();
                client_build = client_build.unix_socket(api_url.path());
                // bw serve 未指定端口时默认端口为 8087
                // reqwest 需要 url 查找 path 如 http://localhost:8087/object/item/xx，host:port
                // 部分会作为 header `Host: $host:$port` 用于服务端检查。
                // 使用 unix socket 需要与 bw serve 的端口一致避免无法通过检查导致 forbidden
                let api_url = format!(
                    "http://{}:{}",
                    api_url.host_str().unwrap_or("localhost"),
                    api_url.port().unwrap_or(8087)
                )
                .parse()?;
                (api_url, client_build)
            }
            "http" | "https" => (api_url, Client::builder()),
            s => {
                bail!("Unsupported scheme {}", s)
            }
        };
        Ok(BWServeApi {
            base_url: api_url,
            client: client_build.build()?,
        })
    }

    async fn parse_resp<T>(resp: reqwest::Response) -> Result<BWServeResp<T>>
    where
        T: DeserializeOwned,
    {
        let status = resp.status();
        // 400 Bad Request.
        // NOTE: /object/item/names-multiple 找到多个时会返回 400 code
        // {success=false, message=xx, data=[item1, item2]}
        let resp = if status == 400 {
            resp
        } else {
            resp.error_for_status()?
        };

        let bytes = resp.bytes().await?;
        if tracing::enabled!(tracing::Level::TRACE) {
            trace!(body_text = %String::from_utf8_lossy(&bytes), "received response body")
        }
        de_json_from_slice::<BWServeResp<T>>(&bytes).map_err(Into::into)
    }

    #[instrument]
    pub async fn get(
        &self,
        args: &BWGetArgs,
    ) -> Result<BWServeResp<BWServeGetRespData>> {
        let url = self.base_url.join(&format!(
            "/object/{}/{}",
            args.object.to_kebab_case(),
            args.id
        ))?;
        debug!(url = %url, "sending GET request");
        let resp = self.client.get(url).send().await?;
        Self::parse_resp(resp).await
    }

    #[instrument]
    pub async fn list(
        &self,
        args: &BwListArgs,
    ) -> Result<BWServeResp<BWServeRespObjectData<BWServeListRespData>>> {
        let mut url = self
            .base_url
            .join(&format!("/list/object/{}", args.object))?;
        let list_url_params = sonic_rs::to_value(args)?
            .as_object()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "list args is not a object for url query args: {args:?}"
                )
            })?
            .iter()
            // List all items in the trash. This query parameter is not a true boolean,
            // in that ?trash, ?trash=true, and ?trash=false will all be interpreted
            // as a request to list items in the trash.
            // 当 bool=false 跳过设置为 key=false 避免被 bw serve 认为其已设置
            .filter(|(_, v)| !v.is_null() && v.as_bool().is_none_or(|b| b))
            .map(|(k, v)| {
                let v_str = match v.as_ref() {
                    ValueRef::String(v) => v,
                    ValueRef::Bool(v) => {
                        if v {
                            "true"
                        } else {
                            "false"
                        }
                    }
                    v => bail!("unknown value type of field {}: {:?}", k, v),
                };
                Ok(format!("{}={}", k, v_str))
            })
            .collect::<Result<Vec<_>>>()?
            .join("&");

        if !list_url_params.is_empty() {
            url.set_query(Some(&list_url_params));
        }

        debug!(url = %url, "sending get request");
        let resp = self.client.get(url).send().await?;
        Self::parse_resp(resp).await
    }

    #[instrument]
    pub async fn status(&self) -> Result<BWServeResp<BWServeStatusRespData>> {
        let url = self.base_url.join("/status")?;
        let resp = self.client.get(url).send().await?;
        Self::parse_resp(resp).await
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case("http://example.com:8080/", "http://example.com:8080")]
    #[case(
        "http://example.com:8080/path/to/",
        "http://example.com:8080/path/to/"
    )]
    #[case("unix:///tmp/bw-serve.sock", "http://localhost:8087")]
    #[case(
        "unix://localhost:18888/tmp/bw-serve.sock",
        "http://localhost:18888"
    )]
    fn new_api_with_url_test(
        #[case] url: &str,
        #[case] expected: &str,
    ) -> Result<()> {
        let api = BWServeApi::new(url)?;
        let expected = expected.parse::<Url>()?;
        assert_eq!(api.base_url, expected);
        Ok(())
    }

    #[rstest]
    #[case("unix://:18888/tmp/bw.sock")]
    // 暂不支持 fd unix sock
    #[case("fd+connected://114")]
    #[case("fd+listening://514")]
    #[case("other-scheme://host")]
    fn new_api_with_url_error_test(#[case] url: &str) {
        let res = BWServeApi::new(url);
        assert!(res.is_err())
    }
}
