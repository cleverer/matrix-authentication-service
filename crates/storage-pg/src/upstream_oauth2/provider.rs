// Copyright 2022, 2023 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use mas_data_model::{UpstreamOAuthProvider, UpstreamOAuthProviderClaimsImports};
use mas_iana::{jose::JsonWebSignatureAlg, oauth::OAuthClientAuthenticationMethod};
use mas_storage::{
    upstream_oauth2::{UpstreamOAuthProviderFilter, UpstreamOAuthProviderRepository},
    Clock, Page, Pagination,
};
use oauth2_types::scope::Scope;
use rand::RngCore;
use sea_query::{enum_def, Expr, PostgresQueryBuilder, Query};
use sea_query_binder::SqlxBinder;
use sqlx::{types::Json, PgConnection};
use tracing::{info_span, Instrument};
use ulid::Ulid;
use uuid::Uuid;

use crate::{
    iden::UpstreamOAuthProviders, pagination::QueryBuilderExt, tracing::ExecuteExt, DatabaseError,
    DatabaseInconsistencyError,
};

/// An implementation of [`UpstreamOAuthProviderRepository`] for a PostgreSQL
/// connection
pub struct PgUpstreamOAuthProviderRepository<'c> {
    conn: &'c mut PgConnection,
}

impl<'c> PgUpstreamOAuthProviderRepository<'c> {
    /// Create a new [`PgUpstreamOAuthProviderRepository`] from an active
    /// PostgreSQL connection
    pub fn new(conn: &'c mut PgConnection) -> Self {
        Self { conn }
    }
}

#[derive(sqlx::FromRow)]
#[enum_def]
struct ProviderLookup {
    upstream_oauth_provider_id: Uuid,
    issuer: String,
    scope: String,
    client_id: String,
    encrypted_client_secret: Option<String>,
    token_endpoint_signing_alg: Option<String>,
    token_endpoint_auth_method: String,
    created_at: DateTime<Utc>,
    claims_imports: Json<UpstreamOAuthProviderClaimsImports>,
}

impl TryFrom<ProviderLookup> for UpstreamOAuthProvider {
    type Error = DatabaseInconsistencyError;
    fn try_from(value: ProviderLookup) -> Result<Self, Self::Error> {
        let id = value.upstream_oauth_provider_id.into();
        let scope = value.scope.parse().map_err(|e| {
            DatabaseInconsistencyError::on("upstream_oauth_providers")
                .column("scope")
                .row(id)
                .source(e)
        })?;
        let token_endpoint_auth_method = value.token_endpoint_auth_method.parse().map_err(|e| {
            DatabaseInconsistencyError::on("upstream_oauth_providers")
                .column("token_endpoint_auth_method")
                .row(id)
                .source(e)
        })?;
        let token_endpoint_signing_alg = value
            .token_endpoint_signing_alg
            .map(|x| x.parse())
            .transpose()
            .map_err(|e| {
                DatabaseInconsistencyError::on("upstream_oauth_providers")
                    .column("token_endpoint_signing_alg")
                    .row(id)
                    .source(e)
            })?;

        Ok(UpstreamOAuthProvider {
            id,
            issuer: value.issuer,
            scope,
            client_id: value.client_id,
            encrypted_client_secret: value.encrypted_client_secret,
            token_endpoint_auth_method,
            token_endpoint_signing_alg,
            created_at: value.created_at,
            claims_imports: value.claims_imports.0,
        })
    }
}

#[async_trait]
impl<'c> UpstreamOAuthProviderRepository for PgUpstreamOAuthProviderRepository<'c> {
    type Error = DatabaseError;

    #[tracing::instrument(
        name = "db.upstream_oauth_provider.lookup",
        skip_all,
        fields(
            db.statement,
            upstream_oauth_provider.id = %id,
        ),
        err,
    )]
    async fn lookup(&mut self, id: Ulid) -> Result<Option<UpstreamOAuthProvider>, Self::Error> {
        let res = sqlx::query_as!(
            ProviderLookup,
            r#"
                SELECT
                    upstream_oauth_provider_id,
                    issuer,
                    scope,
                    client_id,
                    encrypted_client_secret,
                    token_endpoint_signing_alg,
                    token_endpoint_auth_method,
                    created_at,
                    claims_imports as "claims_imports: Json<UpstreamOAuthProviderClaimsImports>"
                FROM upstream_oauth_providers
                WHERE upstream_oauth_provider_id = $1
            "#,
            Uuid::from(id),
        )
        .traced()
        .fetch_optional(&mut *self.conn)
        .await?;

        let res = res
            .map(UpstreamOAuthProvider::try_from)
            .transpose()
            .map_err(DatabaseError::from)?;

        Ok(res)
    }

    #[tracing::instrument(
        name = "db.upstream_oauth_provider.add",
        skip_all,
        fields(
            db.statement,
            upstream_oauth_provider.id,
            upstream_oauth_provider.issuer = %issuer,
            upstream_oauth_provider.client_id = %client_id,
        ),
        err,
    )]
    #[allow(clippy::too_many_arguments)]
    async fn add(
        &mut self,
        rng: &mut (dyn RngCore + Send),
        clock: &dyn Clock,
        issuer: String,
        scope: Scope,
        token_endpoint_auth_method: OAuthClientAuthenticationMethod,
        token_endpoint_signing_alg: Option<JsonWebSignatureAlg>,
        client_id: String,
        encrypted_client_secret: Option<String>,
        claims_imports: UpstreamOAuthProviderClaimsImports,
    ) -> Result<UpstreamOAuthProvider, Self::Error> {
        let created_at = clock.now();
        let id = Ulid::from_datetime_with_source(created_at.into(), rng);
        tracing::Span::current().record("upstream_oauth_provider.id", tracing::field::display(id));

        sqlx::query!(
            r#"
            INSERT INTO upstream_oauth_providers (
                upstream_oauth_provider_id,
                issuer,
                scope,
                token_endpoint_auth_method,
                token_endpoint_signing_alg,
                client_id,
                encrypted_client_secret,
                created_at,
                claims_imports
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        "#,
            Uuid::from(id),
            &issuer,
            scope.to_string(),
            token_endpoint_auth_method.to_string(),
            token_endpoint_signing_alg.as_ref().map(ToString::to_string),
            &client_id,
            encrypted_client_secret.as_deref(),
            created_at,
            Json(&claims_imports) as _,
        )
        .traced()
        .execute(&mut *self.conn)
        .await?;

        Ok(UpstreamOAuthProvider {
            id,
            issuer,
            scope,
            client_id,
            encrypted_client_secret,
            token_endpoint_signing_alg,
            token_endpoint_auth_method,
            created_at,
            claims_imports,
        })
    }

    #[tracing::instrument(
        name = "db.upstream_oauth_provider.delete_by_id",
        skip_all,
        fields(
            db.statement,
            upstream_oauth_provider.id = %id,
        ),
        err,
    )]
    async fn delete_by_id(&mut self, id: Ulid) -> Result<(), Self::Error> {
        // Delete the authorization sessions first, as they have a foreign key
        // constraint on the links and the providers.
        {
            let span = info_span!(
                "db.oauth2_client.delete_by_id.authorization_sessions",
                upstream_oauth_provider.id = %id,
                db.statement = tracing::field::Empty,
            );
            sqlx::query!(
                r#"
                    DELETE FROM upstream_oauth_authorization_sessions
                    WHERE upstream_oauth_provider_id = $1
                "#,
                Uuid::from(id),
            )
            .record(&span)
            .execute(&mut *self.conn)
            .instrument(span)
            .await?;
        }

        // Delete the links next, as they have a foreign key constraint on the
        // providers.
        {
            let span = info_span!(
                "db.oauth2_client.delete_by_id.links",
                upstream_oauth_provider.id = %id,
                db.statement = tracing::field::Empty,
            );
            sqlx::query!(
                r#"
                    DELETE FROM upstream_oauth_links
                    WHERE upstream_oauth_provider_id = $1
                "#,
                Uuid::from(id),
            )
            .record(&span)
            .execute(&mut *self.conn)
            .instrument(span)
            .await?;
        }

        let res = sqlx::query!(
            r#"
                DELETE FROM upstream_oauth_providers
                WHERE upstream_oauth_provider_id = $1
            "#,
            Uuid::from(id),
        )
        .traced()
        .execute(&mut *self.conn)
        .await?;

        DatabaseError::ensure_affected_rows(&res, 1)
    }

    #[tracing::instrument(
        name = "db.upstream_oauth_provider.add",
        skip_all,
        fields(
            db.statement,
            upstream_oauth_provider.id = %id,
            upstream_oauth_provider.issuer = %issuer,
            upstream_oauth_provider.client_id = %client_id,
        ),
        err,
    )]
    #[allow(clippy::too_many_arguments)]
    async fn upsert(
        &mut self,
        clock: &dyn Clock,
        id: Ulid,
        issuer: String,
        scope: Scope,
        token_endpoint_auth_method: OAuthClientAuthenticationMethod,
        token_endpoint_signing_alg: Option<JsonWebSignatureAlg>,
        client_id: String,
        encrypted_client_secret: Option<String>,
        claims_imports: UpstreamOAuthProviderClaimsImports,
    ) -> Result<UpstreamOAuthProvider, Self::Error> {
        let created_at = clock.now();

        let created_at = sqlx::query_scalar!(
            r#"
                INSERT INTO upstream_oauth_providers (
                    upstream_oauth_provider_id,
                    issuer,
                    scope,
                    token_endpoint_auth_method,
                    token_endpoint_signing_alg,
                    client_id,
                    encrypted_client_secret,
                    created_at,
                    claims_imports
                ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                ON CONFLICT (upstream_oauth_provider_id) 
                    DO UPDATE
                    SET
                        issuer = EXCLUDED.issuer,
                        scope = EXCLUDED.scope,
                        token_endpoint_auth_method = EXCLUDED.token_endpoint_auth_method,
                        token_endpoint_signing_alg = EXCLUDED.token_endpoint_signing_alg,
                        client_id = EXCLUDED.client_id,
                        encrypted_client_secret = EXCLUDED.encrypted_client_secret,
                        claims_imports = EXCLUDED.claims_imports
                RETURNING created_at
            "#,
            Uuid::from(id),
            &issuer,
            scope.to_string(),
            token_endpoint_auth_method.to_string(),
            token_endpoint_signing_alg.as_ref().map(ToString::to_string),
            &client_id,
            encrypted_client_secret.as_deref(),
            created_at,
            Json(&claims_imports) as _,
        )
        .traced()
        .fetch_one(&mut *self.conn)
        .await?;

        Ok(UpstreamOAuthProvider {
            id,
            issuer,
            scope,
            client_id,
            encrypted_client_secret,
            token_endpoint_signing_alg,
            token_endpoint_auth_method,
            created_at,
            claims_imports,
        })
    }

    #[tracing::instrument(
        name = "db.upstream_oauth_provider.list",
        skip_all,
        fields(
            db.statement,
        ),
        err,
    )]
    async fn list(
        &mut self,
        _filter: UpstreamOAuthProviderFilter<'_>,
        pagination: Pagination,
    ) -> Result<Page<UpstreamOAuthProvider>, Self::Error> {
        // XXX: the filter is currently ignored, as it does not have any fields
        let (sql, arguments) = Query::select()
            .expr_as(
                Expr::col((
                    UpstreamOAuthProviders::Table,
                    UpstreamOAuthProviders::UpstreamOAuthProviderId,
                )),
                ProviderLookupIden::UpstreamOauthProviderId,
            )
            .expr_as(
                Expr::col((
                    UpstreamOAuthProviders::Table,
                    UpstreamOAuthProviders::Issuer,
                )),
                ProviderLookupIden::Issuer,
            )
            .expr_as(
                Expr::col((UpstreamOAuthProviders::Table, UpstreamOAuthProviders::Scope)),
                ProviderLookupIden::Scope,
            )
            .expr_as(
                Expr::col((
                    UpstreamOAuthProviders::Table,
                    UpstreamOAuthProviders::ClientId,
                )),
                ProviderLookupIden::ClientId,
            )
            .expr_as(
                Expr::col((
                    UpstreamOAuthProviders::Table,
                    UpstreamOAuthProviders::EncryptedClientSecret,
                )),
                ProviderLookupIden::EncryptedClientSecret,
            )
            .expr_as(
                Expr::col((
                    UpstreamOAuthProviders::Table,
                    UpstreamOAuthProviders::TokenEndpointSigningAlg,
                )),
                ProviderLookupIden::TokenEndpointSigningAlg,
            )
            .expr_as(
                Expr::col((
                    UpstreamOAuthProviders::Table,
                    UpstreamOAuthProviders::TokenEndpointAuthMethod,
                )),
                ProviderLookupIden::TokenEndpointAuthMethod,
            )
            .expr_as(
                Expr::col((
                    UpstreamOAuthProviders::Table,
                    UpstreamOAuthProviders::CreatedAt,
                )),
                ProviderLookupIden::CreatedAt,
            )
            .expr_as(
                Expr::col((
                    UpstreamOAuthProviders::Table,
                    UpstreamOAuthProviders::ClaimsImports,
                )),
                ProviderLookupIden::ClaimsImports,
            )
            .from(UpstreamOAuthProviders::Table)
            .generate_pagination(
                (
                    UpstreamOAuthProviders::Table,
                    UpstreamOAuthProviders::UpstreamOAuthProviderId,
                ),
                pagination,
            )
            .build_sqlx(PostgresQueryBuilder);

        let edges: Vec<ProviderLookup> = sqlx::query_as_with(&sql, arguments)
            .traced()
            .fetch_all(&mut *self.conn)
            .await?;

        let page = pagination
            .process(edges)
            .try_map(UpstreamOAuthProvider::try_from)?;

        return Ok(page);
    }

    #[tracing::instrument(
        name = "db.upstream_oauth_provider.count",
        skip_all,
        fields(
            db.statement,
        ),
        err,
    )]
    async fn count(
        &mut self,
        _filter: UpstreamOAuthProviderFilter<'_>,
    ) -> Result<usize, Self::Error> {
        // XXX: the filter is currently ignored, as it does not have any fields
        let (sql, arguments) = Query::select()
            .expr(
                Expr::col((
                    UpstreamOAuthProviders::Table,
                    UpstreamOAuthProviders::UpstreamOAuthProviderId,
                ))
                .count(),
            )
            .from(UpstreamOAuthProviders::Table)
            .build_sqlx(PostgresQueryBuilder);

        let count: i64 = sqlx::query_scalar_with(&sql, arguments)
            .traced()
            .fetch_one(&mut *self.conn)
            .await?;

        count
            .try_into()
            .map_err(DatabaseError::to_invalid_operation)
    }

    #[tracing::instrument(
        name = "db.upstream_oauth_provider.all",
        skip_all,
        fields(
            db.statement,
        ),
        err,
    )]
    async fn all(&mut self) -> Result<Vec<UpstreamOAuthProvider>, Self::Error> {
        let res = sqlx::query_as!(
            ProviderLookup,
            r#"
                SELECT
                    upstream_oauth_provider_id,
                    issuer,
                    scope,
                    client_id,
                    encrypted_client_secret,
                    token_endpoint_signing_alg,
                    token_endpoint_auth_method,
                    created_at,
                    claims_imports as "claims_imports: Json<UpstreamOAuthProviderClaimsImports>"
                FROM upstream_oauth_providers
            "#,
        )
        .traced()
        .fetch_all(&mut *self.conn)
        .await?;

        let res: Result<Vec<_>, _> = res.into_iter().map(TryInto::try_into).collect();
        Ok(res?)
    }
}
