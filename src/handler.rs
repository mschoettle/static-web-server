// SPDX-License-Identifier: MIT OR Apache-2.0
// This file is part of Static Web Server.
// See https://static-web-server.net/ for more information
// Copyright (C) 2019-present Jose Quintana <joseluisq.net>

//! Request handler module intended to manage incoming HTTP requests.
//!

use hyper::{Body, Request, Response, StatusCode};
use std::{future::Future, net::IpAddr, net::SocketAddr, path::PathBuf, sync::Arc};

#[cfg(any(
    feature = "compression",
    feature = "compression-gzip",
    feature = "compression-brotli",
    feature = "compression-zstd",
    feature = "compression-deflate"
))]
use crate::{compression, compression_static};

#[cfg(feature = "basic-auth")]
use crate::basic_auth;

#[cfg(feature = "fallback-page")]
use crate::fallback_page;

#[cfg(all(unix, feature = "experimental"))]
use crate::metrics;

use crate::{
    control_headers, cors, custom_headers, error_page, health,
    http_ext::MethodExt,
    maintenance_mode, redirects, rewrites, security_headers,
    settings::Advanced,
    static_files::{self, HandleOpts},
    virtual_hosts, Error, Result,
};

#[cfg(feature = "directory-listing")]
use crate::directory_listing::DirListFmt;

/// It defines options for a request handler.
pub struct RequestHandlerOpts {
    // General options
    /// Root directory of static files.
    pub root_dir: PathBuf,
    /// Compression feature.
    pub compression: bool,
    /// Compression static feature.
    pub compression_static: bool,
    /// Directory listing feature.
    #[cfg(feature = "directory-listing")]
    #[cfg_attr(docsrs, doc(cfg(feature = "directory-listing")))]
    pub dir_listing: bool,
    /// Directory listing order feature.
    #[cfg(feature = "directory-listing")]
    #[cfg_attr(docsrs, doc(cfg(feature = "directory-listing")))]
    pub dir_listing_order: u8,
    #[cfg(feature = "directory-listing")]
    #[cfg_attr(docsrs, doc(cfg(feature = "directory-listing")))]
    /// Directory listing format feature.
    pub dir_listing_format: DirListFmt,
    /// CORS feature.
    pub cors: Option<cors::Configured>,
    /// Security headers feature.
    pub security_headers: bool,
    /// Cache control headers feature.
    pub cache_control_headers: bool,
    /// Page for 404 errors.
    pub page404: PathBuf,
    /// Page for 50x errors.
    pub page50x: PathBuf,
    /// Page fallback feature.
    #[cfg(feature = "fallback-page")]
    #[cfg_attr(docsrs, doc(cfg(feature = "fallback-page")))]
    pub page_fallback: Vec<u8>,
    /// Basic auth feature.
    #[cfg(feature = "basic-auth")]
    #[cfg_attr(docsrs, doc(cfg(feature = "basic-auth")))]
    pub basic_auth: String,
    /// Index files feature.
    pub index_files: Vec<String>,
    /// Log remote address feature.
    pub log_remote_address: bool,
    /// Redirect trailing slash feature.
    pub redirect_trailing_slash: bool,
    /// Ignore hidden files feature.
    pub ignore_hidden_files: bool,
    /// Health endpoint feature.
    pub health: bool,
    /// Metrics endpoint feature (experimental).
    #[cfg(all(unix, feature = "experimental"))]
    pub experimental_metrics: bool,
    /// Maintenance mode feature.
    pub maintenance_mode: bool,
    /// Custom HTTP status for when entering into maintenance mode.
    pub maintenance_mode_status: StatusCode,
    /// Custom maintenance mode HTML file.
    pub maintenance_mode_file: PathBuf,

    /// Advanced options from the config file.
    pub advanced_opts: Option<Advanced>,
}

impl Default for RequestHandlerOpts {
    fn default() -> Self {
        Self {
            root_dir: PathBuf::from("./public"),
            compression: true,
            compression_static: false,
            #[cfg(feature = "directory-listing")]
            dir_listing: false,
            #[cfg(feature = "directory-listing")]
            dir_listing_order: 6, // unordered
            #[cfg(feature = "directory-listing")]
            dir_listing_format: DirListFmt::Html,
            cors: None,
            security_headers: false,
            cache_control_headers: true,
            page404: PathBuf::from("./404.html"),
            page50x: PathBuf::from("./50x.html"),
            #[cfg(feature = "fallback-page")]
            page_fallback: Vec::new(),
            #[cfg(feature = "basic-auth")]
            basic_auth: String::new(),
            index_files: vec!["index.html".into()],
            log_remote_address: false,
            redirect_trailing_slash: true,
            ignore_hidden_files: false,
            health: false,
            #[cfg(all(unix, feature = "experimental"))]
            experimental_metrics: false,
            maintenance_mode: false,
            maintenance_mode_status: StatusCode::SERVICE_UNAVAILABLE,
            maintenance_mode_file: PathBuf::new(),
            advanced_opts: None,
        }
    }
}

/// It defines the main request handler used by the Hyper service request.
pub struct RequestHandler {
    /// Request handler options.
    pub opts: Arc<RequestHandlerOpts>,
}

impl RequestHandler {
    /// Main entry point for incoming requests.
    pub fn handle<'a>(
        &'a self,
        req: &'a mut Request<Body>,
        remote_addr: Option<SocketAddr>,
    ) -> impl Future<Output = Result<Response<Body>, Error>> + Send + 'a {
        let mut base_path = &self.opts.root_dir;
        #[cfg(feature = "directory-listing")]
        let dir_listing = self.opts.dir_listing;
        #[cfg(feature = "directory-listing")]
        let dir_listing_order = self.opts.dir_listing_order;
        #[cfg(feature = "directory-listing")]
        let dir_listing_format = &self.opts.dir_listing_format;
        let log_remote_addr = self.opts.log_remote_address;
        let redirect_trailing_slash = self.opts.redirect_trailing_slash;
        let compression_static = self.opts.compression_static;
        let ignore_hidden_files = self.opts.ignore_hidden_files;
        let index_files: Vec<&str> = self.opts.index_files.iter().map(|s| s.as_str()).collect();

        // Log request information with its remote address if available
        let mut remote_addr_str = String::new();
        if log_remote_addr {
            remote_addr_str.push_str(" remote_addr=");
            remote_addr_str.push_str(&remote_addr.map_or("".to_owned(), |v| v.to_string()));

            if let Some(client_ip_address) = req
                .headers()
                .get("X-Forwarded-For")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.split(',').next())
                .and_then(|s| s.trim().parse::<IpAddr>().ok())
            {
                remote_addr_str.push_str(" real_remote_ip=");
                remote_addr_str.push_str(&client_ip_address.to_string())
            }
        }

        async move {
            if let Some(result) = health::pre_process(&self.opts, req, &remote_addr_str) {
                return result;
            }

            // Health requests aren't logged here but in health module.
            tracing::info!(
                "incoming request: method={} uri={}{}",
                req.method(),
                req.uri(),
                remote_addr_str,
            );

            // Reject in case of incoming HTTP request method is not allowed
            if !req.method().is_allowed() {
                return error_page::error_response(
                    req.uri(),
                    req.method(),
                    &StatusCode::METHOD_NOT_ALLOWED,
                    &self.opts.page404,
                    &self.opts.page50x,
                );
            }

            // Metrics endpoint check
            #[cfg(all(unix, feature = "experimental"))]
            if let Some(result) = metrics::pre_process(&self.opts, req) {
                return result;
            }

            // CORS
            if let Some(result) = cors::pre_process(&self.opts, req) {
                return result;
            }

            // `Basic` HTTP Authorization Schema
            #[cfg(feature = "basic-auth")]
            if let Some(response) = basic_auth::pre_process(&self.opts, req) {
                return response;
            }

            // Maintenance Mode
            if let Some(response) = maintenance_mode::pre_process(&self.opts, req) {
                return response;
            }

            // Redirects
            if let Some(result) = redirects::pre_process(&self.opts, req) {
                return result;
            }

            // Rewrites
            if let Some(result) = rewrites::pre_process(&self.opts, req) {
                return result;
            }

            // Advanced options
            if let Some(advanced) = &self.opts.advanced_opts {
                // If the "Host" header matches any virtual_host, change the root directory
                if let Some(root) =
                    virtual_hosts::get_real_root(req.headers(), advanced.virtual_hosts.as_deref())
                {
                    base_path = root;
                }
            }

            let index_files = index_files.as_ref();

            // Static files
            let (resp, file_path) = match static_files::handle(&HandleOpts {
                method: req.method(),
                headers: req.headers(),
                base_path,
                uri_path: req.uri().path(),
                uri_query: req.uri().query(),
                #[cfg(feature = "directory-listing")]
                dir_listing,
                #[cfg(feature = "directory-listing")]
                dir_listing_order,
                #[cfg(feature = "directory-listing")]
                dir_listing_format,
                redirect_trailing_slash,
                compression_static,
                ignore_hidden_files,
                index_files,
            })
            .await
            {
                Ok(result) => (result.resp, Some(result.file_path)),
                Err(status) => (
                    error_page::error_response(
                        req.uri(),
                        req.method(),
                        &status,
                        &self.opts.page404,
                        &self.opts.page50x,
                    )?,
                    None,
                ),
            };

            // Check for a fallback response
            #[cfg(feature = "fallback-page")]
            let resp = fallback_page::post_process(&self.opts, req, resp)?;

            // Append CORS headers if they are present
            let resp = cors::post_process(&self.opts, req, resp)?;

            // Add a `Vary` header if static compression is used
            #[cfg(any(
                feature = "compression",
                feature = "compression-gzip",
                feature = "compression-brotli",
                feature = "compression-zstd",
                feature = "compression-deflate"
            ))]
            let resp = compression_static::post_process(&self.opts, req, resp)?;

            // Auto compression based on the `Accept-Encoding` header
            #[cfg(any(
                feature = "compression",
                feature = "compression-gzip",
                feature = "compression-brotli",
                feature = "compression-zstd",
                feature = "compression-deflate"
            ))]
            let resp = compression::post_process(&self.opts, req, resp)?;

            // Append `Cache-Control` headers for web assets
            let resp = control_headers::post_process(&self.opts, req, resp)?;

            // Append security headers
            let resp = security_headers::post_process(&self.opts, req, resp)?;

            // Add/update custom headers
            let resp = custom_headers::post_process(&self.opts, req, resp, file_path.as_ref())?;

            Ok(resp)
        }
    }
}
