use std::{cmp, collections::HashMap, error::Error, sync::Arc};

use async_recursion::async_recursion;
use futures::stream::StreamExt;
use parking_lot::Mutex;

use crate::{
    network::request::Request,
    runner::utils::{FoundParameter, ReasonKind}, utils::progress_style_check_requests,
};

use super::runner::Runner;

/// impl logic for checking parameters
impl<'a> Runner<'a> {
    /// just splits params into two parts and runs check_parameters_recursion for every part
    async fn repeat(
        &self,
        shared_diffs: Arc<Mutex<&'a mut Vec<String>>>,
        shared_green_lines: Arc<Mutex<&'a mut HashMap<String, usize>>>,
        shared_found_params: Arc<Mutex<&'a mut Vec<FoundParameter>>>,
        mut params: Vec<String>,
        recursion_depth: usize,
    ) -> Result<(), Box<dyn Error>> {
        // Prevent stack overflow - limit recursion depth
        if recursion_depth > 50 {
            return Ok(());
        }
        
        // Base case: if we have 1 or fewer parameters, no need to split
        if params.len() <= 1 {
            return self.check_parameters_recursion(
                shared_diffs,
                shared_green_lines,
                shared_found_params,
                params,
                recursion_depth + 1,
            ).await;
        }
        
        let second_params_part = params.split_off(params.len() / 2);

        self.check_parameters_recursion(
            Arc::clone(&shared_diffs),
            Arc::clone(&shared_green_lines),
            Arc::clone(&shared_found_params),
            params,
            recursion_depth + 1,
        )
        .await?;
        self.check_parameters_recursion(
            shared_diffs,
            shared_green_lines,
            shared_found_params,
            second_params_part,
            recursion_depth + 1,
        )
        .await
    }

    #[async_recursion(?Send)]
    async fn check_parameters_recursion(
        &self,
        shared_diffs: Arc<Mutex<&'a mut Vec<String>>>,
        shared_green_lines: Arc<Mutex<&'a mut HashMap<String, usize>>>,
        shared_found_params: Arc<Mutex<&'a mut Vec<FoundParameter>>>,
        mut params: Vec<String>,
        recursion_depth: usize,
    ) -> Result<(), Box<dyn Error>> {
        // Base case: if no parameters left, nothing to check
        if params.is_empty() {
            return Ok(());
        }
        
        // Prevent stack overflow - limit recursion depth  
        if recursion_depth > 50 {
            return Ok(());
        }
        
        let request = Request::new(&self.request_defaults, params.clone());
        let mut response = match request.clone().wrapped_send().await {
            Ok(val) => val,
            Err(_) => match Request::new_random(&self.request_defaults, params.len())
                .send()
                .await
            {
                //we don't return the actual response because it was a random request without original parameters
                //instead we return an empty response from the original request
                Ok(_) => request.empty_response(),
                //looks like either server or network is down
                Err(err) => Err(format!("Unable to reach server ({})", err))?,
            },
        };

        if self.stable.reflections {
            response.fill_reflected_parameters(&self.initial_response);

            let (reflected_parameter, repeat) = response.proceed_reflected_parameters();

            if let Some(reflected_parameter) = reflected_parameter {

                let mut found_params = shared_found_params.lock();
                if !found_params.iter().any(|x| x.name == reflected_parameter) {
                    let mut kind = ReasonKind::Reflected;
                    // explained in response.proceed_reflected_parameters() method
                    // chunk.len() == 1 and not 2 because the random parameter appends later
                    if params.len() == 1 {
                        kind = ReasonKind::NotReflected;
                    }

                    found_params.push(FoundParameter::new(
                        reflected_parameter,
                        &vec![],
                        response.code,
                        response.text.len(),
                        kind.clone(),
                    ));
                    drop(found_params);

                    // remove found parameter from the list
                    params.remove(
                        params
                            .iter()
                            .position(|x| *x == reflected_parameter)
                            .unwrap(),
                    );

                    response.write_and_save(
                        self.id,
                        self.config,
                        &self.initial_response,
                        kind,
                        reflected_parameter,
                        None,
                        self.progress_bar,
                    )?;
                }
            }

            if repeat {
                return self
                    .repeat(
                        shared_diffs,
                        shared_green_lines,
                        shared_found_params,
                        params.clone(),
                        recursion_depth + 1,
                    )
                    .await;
            }

            if self.config.reflected_only {
                return Ok(());
            }
        }

        if self.initial_response.code != response.code {
            // increases the specific response code counter
            // helps to notice whether the page's completely changed
            // like, for example, when the IP got banned by the server
            {
                let mut green_lines = shared_green_lines.lock();
                match green_lines.get(&response.code.to_string()) {
                    Some(val) => {
                        let n_val = *val;
                        green_lines.insert(response.code.to_string(), n_val + 1);
                        if n_val > 50 {
                            drop(green_lines);

                            let check_response =
                                Request::new_random(&self.request_defaults, params.len())
                                    .wrapped_send()
                                    .await
                                    .unwrap_or_default();

                            if check_response.code != self.initial_response.code {
                                return Err(format!(
                                    "{} The page became unstable (code)",
                                    self.request_defaults.url()
                                ))?;
                            } else {
                                let mut green_lines = shared_green_lines.lock();
                                green_lines.insert(response.code.to_string(), 0);
                            }
                        }
                    }
                    _ => {
                        green_lines.insert(response.code.to_string(), 0);
                    }
                }
            }

            // there's only 1 parameter left that's changing the page's code
            if params.len() == 1 {
                response.write_and_save(
                    self.id,
                    self.config,
                    &self.initial_response,
                    ReasonKind::Code,
                    &params[0],
                    None,
                    self.progress_bar,
                )?;

                let mut found_params = shared_found_params.lock();
                found_params.push(FoundParameter::new(
                    &params[0],
                    &vec![format!(
                        "{} -> {}",
                        &self.initial_response.code, response.code
                    )],
                    response.code,
                    response.text.len(),
                    ReasonKind::Code,
                ));
            // there's more than 1 parameter left - split the list and repeat
            } else {
                return self
                    .repeat(
                        shared_diffs,
                        shared_green_lines,
                        shared_found_params,
                        params.clone(),
                        recursion_depth + 1,
                    )
                    .await;
            }
        } else if self.stable.body {
            // check whether the new_diff has at least 1 unique diff compared to stored diffs
            let (_, new_diffs) = {
                let diffs = shared_diffs.lock();
                response.compare(&self.initial_response, &diffs)?
            };

            // and then make a new request to check whether it's a permament diff or not
            if !new_diffs.is_empty() {
                if self.config.strict {
                    let found_params = shared_found_params.lock();
                    if found_params.iter().any(|x| x.diffs == new_diffs.join("|")) {
                        return Ok(());
                    }
                }

                // just request the page with random parameters and store it's diffs
                // maybe I am overcheking this, but still to be sure..
                let tmp_resp = Request::new_random(&self.request_defaults, params.len())
                    .send()
                    .await?;

                let (_, tmp_diffs) = {
                    let diffs = shared_diffs.lock();
                    tmp_resp.compare(&self.initial_response, &diffs)?
                };

                let mut diffs = shared_diffs.lock();
                for diff in tmp_diffs {
                    diffs.push(diff);
                }
            }

            let diffs = shared_diffs.lock();

            // check whether the page still(after making a random request and storing it's diffs) has an unique diffs
            for diff in new_diffs.iter() {
                if !diffs.contains(diff) {
                    let mut found_params = shared_found_params.lock();

                    // there's only one parameter left that changing the page
                    if params.len() == 1 && !found_params.iter().any(|x| x.name == params[0]) {
                        // repeating --strict checks. We need to do it twice because we're usually running in parallel
                        // and some parameters may be found after the first check
                        if self.config.strict && found_params.iter().any(|x| x.diffs == new_diffs.join("|")) {
                            return Ok(());
                        }

                        response.write_and_save(
                            self.id,
                            self.config,
                            &self.initial_response,
                            ReasonKind::Text,
                            &params[0],
                            Some(diff),
                            self.progress_bar,
                        )?;

                        found_params.push(FoundParameter::new(
                            &params[0],
                            &new_diffs,
                            response.code,
                            response.text.len(),
                            ReasonKind::Text,
                        ));
                        break;
                    // we don't know what parameter caused the difference in response yet
                    // so we are repeating
                    } else {
                        drop(diffs);
                        drop(found_params);
                        return self
                            .repeat(
                                shared_diffs,
                                shared_green_lines,
                                shared_found_params,
                                params.clone(),
                                recursion_depth + 1,
                            )
                            .await;
                    }
                }
            }
        }

        Ok(())
    }

    /// check parameters in a loop chunk by chunk
    pub async fn check_parameters(
        &self,
        params: &Vec<String>,
    ) -> Result<(Vec<String>, Vec<FoundParameter>), Box<dyn Error>> {
        let max = cmp::min(self.max, params.len());

        // the amount of requests needed for process all the parameters
        let all = params.len() / max;

        // change and reset the progress bar
        self.prepare_progress_bar(progress_style_check_requests(self.config), all + 1);

        // wrap the variables to share them between futures
        let mut diffs = self.diffs.clone();
        let mut green_lines = HashMap::new();
        let mut found_params = Vec::new();

        let shared_diffs = Arc::new(Mutex::new(&mut diffs));
        let shared_green_lines = Arc::new(Mutex::new(&mut green_lines));
        let shared_found_params = Arc::new(Mutex::new(&mut found_params));

        let _futures_data = futures::stream::iter(params.chunks(max).map(|chunk| {
            let shared_diffs = Arc::clone(&shared_diffs);
            let shared_green_lines = Arc::clone(&shared_green_lines);
            let shared_found_params = Arc::clone(&shared_found_params);

            async move {
                self.progress_bar.inc(1);

                self.check_parameters_recursion(
                    shared_diffs,
                    shared_green_lines,
                    shared_found_params,
                    chunk.to_vec(),
                    0,
                )
                .await
            }
        }))
        .buffer_unordered(self.config.concurrency)
        .collect::<Vec<Result<(), Box<dyn Error>>>>()
        .await;

        Ok((diffs, found_params))
    }
}
