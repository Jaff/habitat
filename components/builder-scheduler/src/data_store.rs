// Copyright (c) 2016-2017 Chef Software Inc. and/or applicable contributors
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

use db::pool::Pool;
use db::migration::Migrator;
use postgres;

use config::Config;
use error::{Result, Error};
use rand::{Rng, thread_rng};

use protocol::jobsrv::{Job, JobState};
use protocol::scheduler::*;
use protobuf::RepeatedField;

// DataStore inherits Send + Sync by virtue of having only one member, the pool itself.
#[derive(Debug, Clone)]
pub struct DataStore {
    pool: Pool,
}

impl DataStore {
    /// Create a new DataStore.
    ///
    /// * Can fail if the pool cannot be created
    /// * Blocks creation of the datastore on the existince of the pool; might wait indefinetly.
    pub fn new(config: &Config) -> Result<DataStore> {
        let pool = Pool::new(&config.datastore_connection_url,
                             config.pool_size,
                             config.datastore_connection_retry_ms,
                             config.datastore_connection_timeout,
                             config.datastore_connection_test)?;
        Ok(DataStore { pool: pool })
    }

    /// Create a new DataStore from a pre-existing pool; useful for testing the database.
    pub fn from_pool(pool: Pool) -> Result<DataStore> {
        Ok(DataStore { pool: pool })
    }

    /// Setup the datastore.
    ///
    /// This includes all the schema and data migrations, along with stored procedures for data
    /// access.
    pub fn setup(&self) -> Result<()> {
        let mut migrator = Migrator::new(&self.pool);
        migrator.setup()?;

        // The groups table
        migrator.migrate("scheduler",
                     r#"CREATE TABLE groups (
                                    id bigint PRIMARY KEY,
                                    group_state text,
                                    created_at timestamptz DEFAULT now(),
                                    updated_at timestamptz
                             )"#)?;

        // The projects table
        migrator.migrate("scheduler",
                     r#"CREATE TABLE projects (
                                     id bigserial PRIMARY KEY,
                                     owner_id bigint,
                                     project_name text,
                                     project_state text,
                                     job_id bigint DEFAULT 0,
                                     created_at timestamptz DEFAULT now(),
                                     updated_at timestamptz
                              )"#)?;

        // Insert a new group into the groups table, and add it's projects to the projects table
        migrator.migrate("scheduler",
                     r#"CREATE OR REPLACE FUNCTION insert_group_v1 (
                                id bigint,
                                project_names text[]
                                ) RETURNS void AS $$
                                    DECLARE
                                      n text;
                                    BEGIN
                                        INSERT INTO groups (id, group_state)
                                        VALUES
                                            (id, 'Pending');

                                        FOREACH n IN ARRAY project_names
                                        LOOP
                                            INSERT INTO projects (owner_id, project_name, project_state)
                                            VALUES
                                                (id, n, 'NotStarted');
                                        END LOOP;
                                    END
                                $$ LANGUAGE plpgsql VOLATILE
                                "#)?;

        // Retrieve a group from the groups table
        migrator.migrate("scheduler",
                     r#"CREATE OR REPLACE FUNCTION get_group_v1 (gid bigint) RETURNS SETOF groups AS $$
                            BEGIN
                              RETURN QUERY SELECT * FROM groups WHERE id = gid;
                              RETURN;
                            END
                            $$ LANGUAGE plpgsql STABLE"#)?;

        // Retrieve the projects for a group
        migrator.migrate("scheduler",
                     r#"CREATE OR REPLACE FUNCTION get_projects_for_group_v1 (gid bigint) RETURNS SETOF projects AS $$
                            BEGIN
                              RETURN QUERY SELECT * FROM projects WHERE owner_id = gid;
                              RETURN;
                            END
                            $$ LANGUAGE plpgsql STABLE"#)?;

        // Retrieve Pending groups, while atomically setting their state to Dispatched
        migrator.migrate("scheduler",
                         r#"CREATE OR REPLACE FUNCTION pending_groups_v1 (integer) RETURNS SETOF groups AS
                                $$
                                DECLARE
                                    r groups % rowtype;
                                BEGIN
                                    FOR r IN
                                        SELECT * FROM groups
                                        WHERE group_state = 'Pending'
                                        ORDER BY created_at ASC
                                        FOR UPDATE SKIP LOCKED
                                        LIMIT $1
                                    LOOP
                                        UPDATE groups SET group_state='Dispatching', updated_at=now() WHERE id=r.id RETURNING * INTO r;
                                        RETURN NEXT r;
                                    END LOOP;
                                  RETURN;
                                END
                                $$ LANGUAGE plpgsql VOLATILE"#)?;

        // Update the state of a group
        migrator.migrate("scheduler",
                         r#"CREATE OR REPLACE FUNCTION set_group_state_v1 (gid bigint, gstate text) RETURNS void AS $$
                            BEGIN
                                UPDATE groups SET group_state=gstate, updated_at=now() WHERE id=gid;
                            END
                         $$ LANGUAGE plpgsql VOLATILE"#)?;

        // Update the state of a project
        migrator.migrate("scheduler",
                          r#"CREATE OR REPLACE FUNCTION set_project_state_v1 (pid bigint, jid bigint, state text) RETURNS void AS $$
                             BEGIN
                                 UPDATE projects SET project_state=state, job_id=jid, updated_at=now() WHERE id=pid;
                             END
                          $$ LANGUAGE plpgsql VOLATILE"#)?;

        migrator.migrate("scheduler",
                         r#"CREATE INDEX pending_groups_index_v1 on groups(created_at) WHERE group_state = 'Pending'"#)?;

        // Retrieve a group project
        migrator.migrate("scheduler",
                  r#"CREATE OR REPLACE FUNCTION find_project_v1 (gid bigint, name text) RETURNS SETOF projects AS $$
                         BEGIN
                           RETURN QUERY SELECT * FROM projects WHERE owner_id = gid AND project_name = name;
                           RETURN;
                         END
                         $$ LANGUAGE plpgsql STABLE"#)?;

        Ok(())
    }

    pub fn create_group(&self, msg: &GroupCreate, project_names: Vec<String>) -> Result<Group> {
        let conn = self.pool.get()?;

        assert!(!project_names.is_empty());

        // TODO - the actual message will be used later for sharding

        // BUG - the insert query should be creating and assigning back a group_id,
        // instead of expecting it to be passed in. The random id is a temporary
        // workaround.
        let mut rng = thread_rng();
        let id = rng.gen::<u64>();

        conn.execute("SELECT insert_group_v1($1, $2)",
                     &[&(id as i64), &project_names])
            .map_err(Error::GroupCreate)?;


        let mut projects = RepeatedField::new();

        for name in project_names {
            let mut project = Project::new();
            project.set_name(name);
            project.set_state(ProjectState::NotStarted);
            projects.push(project);
        }

        let mut group = Group::new();
        group.set_id(id);
        group.set_state(GroupState::Pending);
        group.set_projects(projects);

        debug!("Group created: {:?}", group);

        Ok(group)
    }

    pub fn get_group(&self, msg: &GroupGet) -> Result<Option<Group>> {
        let group_id = msg.get_group_id();

        let conn = self.pool.get()?;
        let rows = &conn.query("SELECT * FROM get_group_v1($1)", &[&(group_id as i64)])
            .map_err(Error::GroupGet)?;

        if rows.is_empty() {
            warn!("Group id {} not found", group_id);
            return Ok(None);
        }

        assert!(rows.len() == 1); // should never have more than one

        let mut group = self.row_to_group(&rows.get(0))?;

        let project_rows = &conn.query("SELECT * FROM get_projects_for_group_v1($1)",
                   &[&(group_id as i64)])
            .map_err(Error::GroupGet)?;

        assert!(project_rows.len() > 0); // should at least have one
        let projects = self.rows_to_projects(&project_rows)?;

        group.set_projects(projects);
        Ok(Some(group))
    }

    fn row_to_group(&self, row: &postgres::rows::Row) -> Result<Group> {
        let mut group = Group::new();

        let id: i64 = row.get("id");
        group.set_id(id as u64);
        let js: String = row.get("group_state");
        let group_state = match &js[..] {
            "Dispatching" => GroupState::Dispatching,
            "Pending" => GroupState::Pending,
            "Complete" => GroupState::Complete,
            "Failed" => GroupState::Failed,
            _ => return Err(Error::UnknownGroupState),
        };
        group.set_state(group_state);

        Ok(group)
    }

    fn row_to_project(&self, row: &postgres::rows::Row) -> Result<Project> {
        let mut project = Project::new();

        let name: String = row.get("project_name");
        let state: String = row.get("project_state");
        let job_id: i64 = row.get("job_id");

        let project_state = match &state[..] {
            "NotStarted" => ProjectState::NotStarted,
            "InProgress" => ProjectState::InProgress,
            "Success" => ProjectState::Success,
            "Failure" => ProjectState::Failure,
            _ => return Err(Error::UnknownProjectState),
        };

        project.set_name(name);
        project.set_state(project_state);
        project.set_job_id(job_id as u64);

        Ok(project)
    }

    fn rows_to_projects(&self, rows: &postgres::rows::Rows) -> Result<RepeatedField<Project>> {
        let mut projects = RepeatedField::new();

        for row in rows {
            let project = self.row_to_project(&row)?;
            projects.push(project);
        }

        Ok(projects)
    }

    pub fn set_group_state(&self, group_id: u64, group_state: GroupState) -> Result<()> {
        let conn = self.pool.get()?;
        let state = match group_state {
            GroupState::Dispatching => "Dispatching",
            GroupState::Pending => "Pending",
            GroupState::Complete => "Complete",
            GroupState::Failed => "Failed",
        };
        conn.execute("SELECT set_group_state_v1($1, $2)",
                     &[&(group_id as i64), &state])
            .map_err(Error::GroupSetState)?;
        Ok(())
    }

    pub fn set_group_job_state(&self, job: &Job) -> Result<()> {
        let conn = self.pool.get()?;
        let rows = &conn.query("SELECT * FROM find_project_v1($1, $2)",
                   &[&(job.get_owner_id() as i64), &job.get_project().get_name()])
            .map_err(Error::ProjectSetState)?;

        // No rows is ok, as this job might not be one we care about
        if rows.is_empty() {
            warn!("No project found for job id: {}", job.get_id());
            return Ok(());
        }

        assert!(rows.len() == 1); // should never have more than one
        let pid: i64 = rows.get(0).get("id");

        let state = match job.get_state() {
            JobState::Complete => "Success",
            JobState::Failed | JobState::Rejected => "Failure",
            _ => "InProgress",
        };

        conn.execute("SELECT set_project_state_v1($1, $2, $3)",
                     &[&pid, &(job.get_id() as i64), &state])
            .map_err(Error::ProjectSetState)?;

        Ok(())
    }

    pub fn pending_groups(&self, count: i32) -> Result<Vec<Group>> {
        let mut groups = Vec::new();

        let conn = self.pool.get()?;
        let group_rows = &conn.query("SELECT * FROM pending_groups_v1($1)", &[&count])
            .map_err(Error::GroupPending)?;

        for group_row in group_rows {
            let mut group = self.row_to_group(&group_row)?;

            let project_rows = &conn.query("SELECT * FROM get_projects_for_group_v1($1)",
                       &[&(group.get_id() as i64)])
                .map_err(Error::GroupPending)?;
            let projects = self.rows_to_projects(&project_rows)?;

            group.set_projects(projects);
            groups.push(group);
        }

        Ok(groups)
    }
}
