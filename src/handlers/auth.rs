use axum::{
    extract::State,
    response::{Html, IntoResponse, Redirect},
    Form,
};
use axum_extra::extract::cookie::{Cookie, CookieJar};
use serde::Deserialize;

use crate::{
    db::get_setting,
    error::AppResult,
    models::User,
    AppState,
};

pub async fn get_login(
    State(state): State<AppState>,
    jar: CookieJar,
) -> AppResult<impl IntoResponse> {
    // Already logged in → home
    if jar.get("user_id").is_some() {
        return Ok(Redirect::to("/").into_response());
    }

    let users = sqlx::query_as::<_, User>("SELECT * FROM users ORDER BY name")
        .fetch_all(&state.db)
        .await?;

    let theme = get_setting(&state.db, "theme")
        .await
        .unwrap_or_else(|_| "light".to_string());

    let mut ctx = tera::Context::new();
    ctx.insert("users", &users);
    ctx.insert("theme", &theme);
    ctx.insert("is_dark", &(theme == "dark"));

    let html = state.tera.render("auth/login.html", &ctx)?;
    Ok(Html(html).into_response())
}

#[derive(Deserialize)]
pub struct LoginForm {
    pub user_id: i64,
}

pub async fn post_login(
    jar: CookieJar,
    Form(form): Form<LoginForm>,
) -> impl IntoResponse {
    let cookie = Cookie::build(("user_id", form.user_id.to_string()))
        .path("/")
        .http_only(true)
        .build();
    (jar.add(cookie), Redirect::to("/"))
}

pub async fn post_logout(jar: CookieJar) -> impl IntoResponse {
    let jar = jar.remove(Cookie::from("user_id"));
    (jar, Redirect::to("/login"))
}
