use crate::{ggst_api, schema, CHAR_NAMES};

use bb8_redis::redis;
use diesel::prelude::*;
use diesel_async::{AsyncConnection, AsyncPgConnection, RunQueryDsl};
use std::time::Duration;
use tokio::time;
use tracing::{debug, error, info};

use crate::schema::{character_ranks, games, global_ranks, player_names, player_ratings, players};
use chrono::{NaiveDateTime, Utc};
use diesel::dsl::*;

use crate::models::*;

use diesel_async::scoped_futures::ScopedFutureExt;

use diesel::sql_types::{BigInt, Integer};

define_sql_function! {
    fn coalesce(x: diesel::sql_types::Nullable<diesel::sql_types::Timestamp>, y: diesel::sql_types::Timestamp) -> diesel::sql_types::Timestamp;
}

pub const ONE_MINUTE: u64 = 1 * 60;

//TODO move the db stuff from this file into db.rs and imdb.rs

pub async fn pull_and_update_continuous(state: crate::AppState) {
    // Processing loop
    let processing_state = state.clone();
    let processing_task = tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(ONE_MINUTE - 10));

        loop {
            interval.tick().await;

            info!("Processing");

            let mut connection = processing_state.db_pool.get().await.unwrap();
            let mut redis_connection = processing_state.redis_pool.get().await.unwrap();

            if let Err(e) = connection
                .transaction::<_, diesel::result::Error, _>(|conn| {
                    async move {
                        if let Err(e) = update_ratings(conn).await {
                            error!("Rating update failed: {e}");
                        }
                        Ok(())
                    }
                    .scope_boxed()
                })
                .await
            {
                error!("Rating update failed: {e}");
            }

            //Get last_update_hourly from redis
            let last_update_hourly: Result<String, redis::RedisError> = redis::cmd("GET")
                .arg("last_update_hourly")
                .query_async(&mut *redis_connection)
                .await;

            let last_update_hourly = match last_update_hourly {
                Ok(s) => s,
                Err(_) => {
                    //If last_update_hourly doesn't exist, initialize to now minus 2 hours
                    chrono::DateTime::from_timestamp(
                        chrono::Utc::now().naive_utc().and_utc().timestamp() - (2 * 60 * 60),
                        0,
                    )
                    .unwrap()
                    .naive_utc()
                    .to_string()
                }
            };

            //Parse last_update_hourly
            let last_update_hourly =
                NaiveDateTime::parse_from_str(&last_update_hourly, "%Y-%m-%d %H:%M:%S").unwrap();

            //Only do this if "last_update_hourly" from redis is more than an hour ago
            if last_update_hourly < Utc::now().naive_utc() - chrono::Duration::hours(1) {
                info!("Hourly update");
                if let Err(e) = connection
                    .transaction::<_, diesel::result::Error, _>(|conn| {
                        async move {
                            do_hourly_update(conn, &mut redis_connection).await.unwrap();
                            Ok(())
                        }
                        .scope_boxed()
                    })
                    .await
                {
                    error!("Hourly update failed: {e}");
                } else {
                    info!("Hourly update - Done");
                }
            }

            let mut redis_connection = processing_state.redis_pool.get().await.unwrap();

            //Get last_update_daily from redis
            let last_update_daily: Result<String, redis::RedisError> = redis::cmd("GET")
                .arg("last_update_daily")
                .query_async(&mut *redis_connection)
                .await;

            let last_update_daily = match last_update_daily {
                Ok(s) => s,
                Err(_) => {
                    //If last_update_daily doesn't exist, initialize to now minus 2 days
                    chrono::DateTime::from_timestamp(
                        chrono::Utc::now().naive_utc().and_utc().timestamp() - 172800,
                        0,
                    )
                    .unwrap()
                    .naive_utc()
                    .to_string()
                }
            };

            //Parse last_update_daily
            let last_update_daily =
                NaiveDateTime::parse_from_str(&last_update_daily, "%Y-%m-%d %H:%M:%S").unwrap();

            if last_update_daily < Utc::now().naive_utc() - chrono::Duration::days(1) {
                info!("Daily update");
                if let Err(e) = connection
                    .transaction::<_, diesel::result::Error, _>(|conn| {
                        async move {
                            do_daily_update(conn, &mut redis_connection).await.unwrap();
                            Ok(())
                        }
                        .scope_boxed()
                    })
                    .await
                {
                    error!("Daily update failed: {e}");
                } else {
                    info!("Daily update - Done");
                }
            };

            info!("Processing - done");
        }
    });

    // Replay pulling loop
    let pull_state = state.clone();
    let pull_task = tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(ONE_MINUTE));

        loop {
            interval.tick().await;

            info!("Replay pull");

            let mut connection = pull_state.db_pool.get().await.unwrap();
            let mut redis_connection = pull_state.redis_pool.get().await.unwrap();

            if let Err(e) = connection
                .transaction::<_, diesel::result::Error, _>(|conn| {
                    async move {
                        match grab_games(conn, &mut redis_connection).await {
                            Ok(new_games) => {
                                info!("New games: {:?}", new_games.len());
                            }
                            Err(e) => {
                                error!("grab_games failed: {e}");
                            }
                        };
                        Ok(())
                    }
                    .scope_boxed()
                })
                .await
            {
                error!("Replay pull loop: {e}");
            }

            info!("Replay pull - Done");
        }
    });

    tokio::select! {
        _ = processing_task => {},
        _ = pull_task => {},
    }
}

pub async fn do_hourly_update_once(state: crate::AppState) {
    let mut connection = state.db_pool.get().await.unwrap();
    let mut redis_connection = state.redis_pool.get().await.unwrap();

    connection
        .transaction::<_, diesel::result::Error, _>(|conn| {
            async move {
                do_hourly_update(conn, &mut redis_connection).await.unwrap();
                Ok(())
            }
            .scope_boxed()
        })
        .await
        .unwrap();
}

pub async fn do_daily_update_once(state: crate::AppState) {
    let mut connection = state.db_pool.get().await.unwrap();
    let mut redis_connection = state.redis_pool.get().await.unwrap();

    connection
        .transaction::<_, diesel::result::Error, _>(|conn| {
            async move {
                do_daily_update(conn, &mut redis_connection).await.unwrap();
                Ok(())
            }
            .scope_boxed()
        })
        .await
        .unwrap();
}

async fn do_hourly_update(
    conn: &mut crate::Connection<'_>,
    redis_connection: &mut crate::RedisConnection<'_>,
) -> Result<(), String> {
    if let Err(e) = decay(conn).await {
        error!("decay failed: {e}");
    }

    if let Err(e) = update_ranks(conn).await {
        error!("update_ranks failed: {e}");
    }

    if let Err(e) = update_stats(conn, redis_connection).await {
        error!("update_stats failed: {e}");
    }

    //Now
    let last_update =
        chrono::DateTime::from_timestamp(chrono::Utc::now().naive_utc().and_utc().timestamp(), 0)
            .unwrap()
            .naive_utc()
            .to_string();

    redis::cmd("SET")
        .arg("last_update_hourly")
        .arg(last_update)
        .query_async::<String>(&mut **redis_connection)
        .await
        .expect("Error setting last_update_hourly");
    Ok(())
}

async fn do_daily_update(
    conn: &mut crate::Connection<'_>,
    redis_connection: &mut crate::RedisConnection<'_>,
) -> Result<(), String> {
    if let Err(e) = update_popularity(conn, redis_connection).await {
        error!("update_popularity failed: {e}");
    }

    if let Err(e) = update_matchups(conn, redis_connection).await {
        error!("update_matchups failed: {e}");
    }

    if let Err(e) = update_distribution(conn, redis_connection).await {
        error!("update_distribution failed: {e}");
    }

    //Now
    let last_update =
        chrono::DateTime::from_timestamp(chrono::Utc::now().naive_utc().and_utc().timestamp(), 0)
            .unwrap()
            .naive_utc()
            .to_string();

    redis::cmd("SET")
        .arg("last_update_daily")
        .arg(last_update)
        .query_async::<String>(&mut **redis_connection)
        .await
        .expect("Error setting last_update_daily");

    //Clear this so that health check doesn't fail
    crate::imdb::clear_latest_game_time(redis_connection)
        .await
        .unwrap();
    Ok(())
}

#[derive(QueryableByName, serde::Serialize, serde::Deserialize)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct DistributionResult {
    #[diesel(sql_type = diesel::sql_types::Integer)]
    pub lower_bound: i32,
    #[diesel(sql_type = diesel::sql_types::Integer)]
    pub upper_bound: i32,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    pub count: i64,
    #[diesel(sql_type = diesel::sql_types::Double)]
    pub percentage: f64,
    #[diesel(sql_type = diesel::sql_types::Double)]
    pub percentile: f64,
}
async fn update_distribution(
    conn: &mut crate::Connection<'_>,
    redis_connection: &mut crate::RedisConnection<'_>,
) -> Result<(), String> {
    info!("Updating distribution");

    let distribution_results = diesel::sql_query(
        "
        WITH buckets AS (
            SELECT generate_series(-500, 2500, 100) AS lower_bound,
                  generate_series(-400, 2600, 100) AS upper_bound
        ),
        bucket_counts AS (  -- CTE to count values in each bucket
            SELECT 
                b.lower_bound, 
                b.upper_bound, 
                count(t.value) AS bucket_count
            FROM player_ratings t
            LEFT JOIN buckets b ON t.value >= b.lower_bound AND t.value < b.upper_bound
            WHERE t.deviation < 30.0
            GROUP BY b.lower_bound, b.upper_bound
        ),
        percentiles AS ( -- CTE to calculate cumulative percentage
            SELECT 
                lower_bound,
                upper_bound,
                bucket_count,
                SUM(bucket_count) OVER (ORDER BY lower_bound) as cumulative_sum,
                SUM(bucket_count) OVER () as total_count
            FROM bucket_counts        
        )
        SELECT 
            p.lower_bound,
            p.upper_bound,
            p.bucket_count AS count,
            CAST(ROUND((p.bucket_count * 100.0 / sum(p.bucket_count) OVER ()), 2) AS FLOAT) AS percentage,
            CAST(ROUND((p.cumulative_sum * 100.0 / p.total_count), 2) AS FLOAT) AS percentile
        FROM percentiles p
        ORDER BY p.lower_bound;
        ",
    );

    let distribution_results = distribution_results
        .get_results::<DistributionResult>(conn)
        .await
        .unwrap();

    redis::cmd("SET")
        .arg("distribution_rating")
        .arg(serde_json::to_string(&distribution_results).unwrap())
        .query_async::<String>(&mut **redis_connection)
        .await
        .expect("Error setting distribution");

    let floor_distribution = schema::games::table
        .filter(
            schema::games::timestamp
                .gt(chrono::Utc::now().naive_utc() - chrono::Duration::days(30)),
        )
        .group_by(schema::games::game_floor)
        .select((schema::games::game_floor, count_star()))
        .get_results::<(i16, i64)>(conn)
        .await
        .expect("Error loading games");

    redis::cmd("SET")
        .arg("distribution_floor")
        .arg(serde_json::to_string(&floor_distribution).unwrap())
        .query_async::<String>(&mut **redis_connection)
        .await
        .expect("Error setting floor_distribution");

    info!("Updating distribution - Done");
    Ok(())
}

#[derive(QueryableByName, serde::Serialize, serde::Deserialize)]
pub struct Matchup {
    #[diesel(sql_type = diesel::sql_types::SmallInt)]
    pub opponent_char: i16,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    pub wins: i64,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    pub total_games: i64,
}
async fn update_matchups(
    conn: &mut crate::Connection<'_>,
    redis_connection: &mut crate::RedisConnection<'_>,
) -> Result<(), String> {
    info!("Updating matchups");

    {
        let mut all_characters: std::collections::HashMap<i16, Vec<Matchup>> =
            std::collections::HashMap::new();

        for c in 0..CHAR_NAMES.len() {
            let results = diesel::sql_query(
                "
              SELECT 
                  opponent_char,
                  SUM(CASE 
                      WHEN (position = 'a' AND winner = 1) OR (position = 'b' AND winner = 2)
                      THEN 1 
                      ELSE 0 
                  END) as wins,
                  COUNT(*) as total_games
              FROM (
                  SELECT 
                      char_b as opponent_char, 
                      winner,
                      'a' as position
                  FROM games
                  WHERE char_a = $1
                  AND timestamp > now() - interval '1 month'
                  AND deviation_a < 30.0
                  AND deviation_b < 30.0
                  UNION ALL
                  SELECT 
                      char_a as opponent_char, 
                      winner,
                      'b' as position
                  FROM games
                  WHERE char_b = $1
                  AND timestamp > now() - interval '1 month'
                  AND deviation_a < 30.0
                  AND deviation_b < 30.0
              ) as combined_results
              GROUP BY opponent_char
              ORDER BY opponent_char;
              ",
            );
            let results: Vec<Matchup> = results
                .bind::<Integer, _>(i32::try_from(c).unwrap())
                .get_results(conn)
                .await
                .unwrap();

            all_characters.insert(i16::try_from(c).unwrap(), results);
        }

        for (char_id, matchups) in all_characters {
            let char_id = char_id as usize;

            redis::cmd("SET")
                .arg(format!("matchup_{}", char_id))
                .arg(serde_json::to_string(&matchups).unwrap())
                .query_async::<String>(&mut **redis_connection)
                .await
                .expect("Error setting matchup");
        }
    }

    {
        let mut all_characters: std::collections::HashMap<i16, Vec<Matchup>> =
            std::collections::HashMap::new();

        for c in 0..CHAR_NAMES.len() {
            let results = diesel::sql_query(
                "
            SELECT 
                opponent_char,
                SUM(CASE 
                    WHEN (position = 'a' AND winner = 1) OR (position = 'b' AND winner = 2)
                    THEN 1 
                    ELSE 0 
                END) as wins,
                COUNT(*) as total_games
            FROM (
                SELECT 
                    char_b as opponent_char, 
                    winner,
                    'a' as position
                FROM games
                WHERE char_a = $1
                AND timestamp > now() - interval '1 month'
                AND deviation_a < 30.0
                AND deviation_b < 30.0
                AND value_a > 1700
                AND value_b > 1700
                UNION ALL
                SELECT 
                    char_a as opponent_char, 
                    winner,
                    'b' as position
                FROM games
                WHERE char_b = $1
                AND timestamp > now() - interval '1 month'
                AND deviation_a < 30.0
                AND deviation_b < 30.0
                AND value_a > 1700
                AND value_b > 1700
            ) as combined_results
            GROUP BY opponent_char
            ORDER BY opponent_char;
            ",
            );
            let results: Vec<Matchup> = results
                .bind::<Integer, _>(i32::try_from(c).unwrap())
                .get_results(conn)
                .await
                .unwrap();

            all_characters.insert(i16::try_from(c).unwrap(), results);
        }

        for (char_id, matchups) in all_characters {
            let char_id = char_id as usize;

            redis::cmd("SET")
                .arg(format!("matchup_1700_{}", char_id))
                .arg(serde_json::to_string(&matchups).unwrap())
                .query_async::<String>(&mut **redis_connection)
                .await
                .expect("Error setting matchup");
        }
    }

    info!("Updating matchups - Done");
    Ok(())
}

#[derive(QueryableByName)]
struct PopularityResult {
    #[diesel(sql_type = diesel::sql_types::SmallInt)]
    c: i16,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    count: i64,
}
#[derive(QueryableByName)]
struct PopularityResultTotal {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    count: i64,
}
async fn update_popularity(
    conn: &mut crate::Connection<'_>,
    redis_connection: &mut crate::RedisConnection<'_>,
) -> Result<(), String> {
    info!("Updating popularity");
    //We're using subqueries here, so we need to use sql_query

    //Distinct player + character combination counts
    let results = diesel::sql_query(
        "
        SELECT c, COUNT(id) as count
        FROM (
            SELECT g.char_a as c, g.id_a as id
            FROM games g
            WHERE g.timestamp > now() - interval '1 month'
        UNION
            SELECT g.char_b as c, g.id_b as id
            FROM games g
            WHERE g.timestamp > now() - interval '1 month'
        ) as combined_results
        GROUP BY c;
    ",
    );

    let results: Vec<PopularityResult> =
        results.get_results::<PopularityResult>(conn).await.unwrap();

    for r in results {
        let char_id = r.c as usize;
        let count = r.count;

        redis::cmd("SET")
            .arg(format!("popularity_per_player_{}", CHAR_NAMES[char_id].0))
            .arg(count)
            .query_async::<String>(&mut **redis_connection)
            .await
            .expect("Error setting popularity per player");
    }

    //Total distinct player + character combination.
    let results_total_players = diesel::sql_query(
        "
        SELECT COUNT(id) as count
        FROM (
            SELECT g.id_a as id
            FROM games g
            WHERE g.timestamp > now() - interval '1 month'
        UNION
            SELECT g.id_b as id
            FROM games g
            WHERE g.timestamp > now() - interval '1 month'
        ) as combined_results;
        ",
    );

    let results_total_players: Vec<PopularityResultTotal> = results_total_players
        .get_results::<PopularityResultTotal>(conn)
        .await
        .unwrap();

    redis::cmd("SET")
        .arg("popularity_per_player_total")
        .arg(results_total_players[0].count)
        .query_async::<String>(&mut **redis_connection)
        .await
        .expect("Error setting popularity_total");

    //Total game count per character (no total needed, we can use 'one_month_games' from stats)
    let results = diesel::sql_query(
        "
        SELECT c, COUNT(c) as count
        FROM (
            SELECT g.char_a as c
            FROM games g
            WHERE g.timestamp > now() - interval '1 month'
        UNION ALL
            SELECT g.char_b as c
            FROM games g
            WHERE g.timestamp > now() - interval '1 month'
        ) as combined_results
        GROUP BY c;
        ",
    );

    let results: Vec<PopularityResult> =
        results.get_results::<PopularityResult>(conn).await.unwrap();

    for r in results {
        let char_id = r.c as usize;
        let count = r.count;

        redis::cmd("SET")
            .arg(format!(
                "popularity_per_character_{}",
                CHAR_NAMES[char_id].0
            ))
            .arg(count)
            .query_async::<String>(&mut **redis_connection)
            .await
            .expect("Error setting popularity per game");
    }

    info!("Updating popularity - Done");
    Ok(())
}

#[derive(QueryableByName, Queryable)]
#[diesel(check_for_backend(diesel::pg::Pg))]
struct CountResult {
    #[diesel(sql_type = BigInt)]
    count: i64,
}
async fn update_stats(
    conn: &mut crate::Connection<'_>,
    redis_connection: &mut crate::RedisConnection<'_>,
) -> Result<(), String> {
    info!("Updating stats");

    // Get total game count
    let total_games: i64 = schema::games::table
        .count()
        .get_result::<i64>(conn)
        .await
        .expect("Error loading games");

    // Get one month game count
    let one_month_games = schema::games::table
        .filter(
            schema::games::timestamp
                .gt(chrono::Utc::now().naive_utc() - chrono::Duration::days(30)),
        )
        .count()
        .get_result::<i64>(conn)
        .await
        .expect("Error loading games");

    // Get one week game count
    let one_week_games = schema::games::table
        .filter(
            schema::games::timestamp.gt(chrono::Utc::now().naive_utc() - chrono::Duration::days(7)),
        )
        .count()
        .get_result::<i64>(conn)
        .await
        .expect("Error loading games");

    // Get one day game count
    let one_day_games = schema::games::table
        .filter(
            schema::games::timestamp.gt(chrono::Utc::now().naive_utc() - chrono::Duration::days(1)),
        )
        .count()
        .get_result::<i64>(conn)
        .await
        .expect("Error loading games");

    // Get one hour game count
    let one_hour_games = schema::games::table
        .filter(
            schema::games::timestamp
                .gt(chrono::Utc::now().naive_utc() - chrono::Duration::hours(1)),
        )
        .count()
        .get_result::<i64>(conn)
        .await
        .expect("Error loading games");

    // Get total player count
    let total_players = schema::players::table
        .count()
        .get_result::<i64>(conn)
        .await
        .expect("Error loading players");

    // Get past month player count
    let one_month_players = diesel::sql_query(
        "
        select count(id)
        from (
            select id_a as id
            from games
            where timestamp > now() - interval '1 month'
            union
            select id_b as id
            from games
            where timestamp > now() - interval '1 month'
        ) as combined_result;
        ",
    );
    let one_month_players = one_month_players
        .get_results::<CountResult>(conn)
        .await
        .unwrap();
    let one_month_players = one_month_players[0].count;

    // Get past month player count
    let one_week_players = diesel::sql_query(
        "
        select count(id)
        from (
            select id_a as id
            from games
            where timestamp > now() - interval '1 week'
            union
            select id_b as id
            from games
            where timestamp > now() - interval '1 week'
        ) as combined_result;
        ",
    );
    let one_week_players = one_week_players
        .get_results::<CountResult>(conn)
        .await
        .unwrap();
    let one_week_players = one_week_players[0].count;

    // Get past day player count
    let one_day_players = diesel::sql_query(
        "
        select count(id)
        from (
            select id_a as id
            from games
            where timestamp > now() - interval '1 day'
            union
            select id_b as id
            from games
            where timestamp > now() - interval '1 day'
        ) as combined_result;
        ",
    );
    let one_day_players = one_day_players
        .get_results::<CountResult>(conn)
        .await
        .unwrap();
    let one_day_players = one_day_players[0].count;

    // Get past hour player count
    let one_hour_players = diesel::sql_query(
        "
        select count(id)
        from (
            select id_a as id
            from games
            where timestamp > now() - interval '1 hour'
            union
            select id_b as id
            from games
            where timestamp > now() - interval '1 hour'
        ) as combined_result;
        ",
    );
    let one_hour_players = one_hour_players
        .get_results::<CountResult>(conn)
        .await
        .unwrap();
    let one_hour_players = one_hour_players[0].count;

    redis::cmd("SET")
        .arg("total_games")
        .arg(total_games)
        .query_async::<String>(&mut **redis_connection)
        .await
        .expect("Error setting total_games");

    redis::cmd("SET")
        .arg("one_month_games")
        .arg(one_month_games)
        .query_async::<String>(&mut **redis_connection)
        .await
        .expect("Error setting one_month_games");

    redis::cmd("SET")
        .arg("one_week_games")
        .arg(one_week_games)
        .query_async::<String>(&mut **redis_connection)
        .await
        .expect("Error setting one_week_games");

    redis::cmd("SET")
        .arg("one_day_games")
        .arg(one_day_games)
        .query_async::<String>(&mut **redis_connection)
        .await
        .expect("Error setting one_day_games");

    redis::cmd("SET")
        .arg("one_hour_games")
        .arg(one_hour_games)
        .query_async::<String>(&mut **redis_connection)
        .await
        .expect("Error setting one_hour_games");

    redis::cmd("SET")
        .arg("total_players")
        .arg(total_players)
        .query_async::<String>(&mut **redis_connection)
        .await
        .expect("Error setting total_players");

    redis::cmd("SET")
        .arg("one_month_players")
        .arg(one_month_players)
        .query_async::<String>(&mut **redis_connection)
        .await
        .expect("Error setting one_month_players");

    redis::cmd("SET")
        .arg("one_week_players")
        .arg(one_week_players)
        .query_async::<String>(&mut **redis_connection)
        .await
        .expect("Error setting one_week_players");

    redis::cmd("SET")
        .arg("one_day_players")
        .arg(one_day_players)
        .query_async::<String>(&mut **redis_connection)
        .await
        .expect("Error setting one_day_players");

    redis::cmd("SET")
        .arg("one_hour_players")
        .arg(one_hour_players)
        .query_async::<String>(&mut **redis_connection)
        .await
        .expect("Error setting one_hour_players");

    info!("Updating stats - Done");
    Ok(())
}

async fn decay(connection: &mut AsyncPgConnection) -> Result<(), String> {
    info!("Decaying ratings");

    diesel::update(player_ratings::table)
        .set((
            player_ratings::deviation.eq((player_ratings::deviation * 1.003) + 0.01),
            player_ratings::last_decay.eq(Utc::now().naive_utc()),
        ))
        .filter(player_ratings::deviation.lt(250.0))
        .execute(connection)
        .await
        .unwrap();

    info!("Decaying ratings - Done");
    Ok(())
}

#[allow(dead_code)]
#[derive(QueryableByName)]
struct InsertedRankRowId {
    #[diesel(sql_type = Integer)]
    rank: i32,
}
async fn update_ranks(connection: &mut AsyncPgConnection) -> Result<(), String> {
    info!("Updating ranks");
    //TODO change this to use Redis?

    //Delete all rows in the global_ranks and character_ranks tables
    diesel::delete(global_ranks::table)
        .execute(connection)
        .await
        .unwrap();
    diesel::delete(character_ranks::table)
        .execute(connection)
        .await
        .unwrap();

    let results = sql_query(
        "
        INSERT INTO global_ranks (rank, id, char_id)
        SELECT ROW_NUMBER()
        OVER (ORDER BY value DESC) as rank, r.id, r.char_id
        FROM player_ratings r
        LEFT JOIN players p ON p.id = r.id
        WHERE r.deviation < 30.0
        AND p.status = 'public'
        ORDER BY value DESC
        LIMIT 1000  
        RETURNING rank
    ",
    );
    let results: Vec<InsertedRankRowId> = results.get_results(connection).await.unwrap();

    info!("Inserted {} rows into global_ranks", results.len());

    for c in 0..CHAR_NAMES.len() {
        let results = sql_query(
            "
            INSERT INTO character_ranks (rank, id, char_id)
            SELECT ROW_NUMBER() 
            OVER (ORDER BY value DESC) as rank, r.id, char_id
            FROM player_ratings r, players p
            WHERE r.id = p.id
            AND p.status = 'public'
            AND deviation < 30.0 AND char_id = $1
            ORDER BY value DESC
            LIMIT 1000
            RETURNING rank
        ",
        );
        let results: Vec<InsertedRankRowId> = results
            .bind::<Integer, _>(i32::try_from(c).unwrap())
            .get_results(connection)
            .await
            .unwrap();

        info!(
            "Inserted {} rows into character_ranks for character {}",
            results.len(),
            CHAR_NAMES[c].1
        );
    }

    info!("Updating ranks - Done");
    Ok(())
}

async fn update_player_info(
    connection: &mut AsyncPgConnection,
    new_game: &Game,
) -> Result<(), String> {
    //Update player name in the player table
    insert_into(players::table)
        .values(&Player {
            id: new_game.id_a,
            name: new_game.name_a.clone(),
            platform: new_game.platform_a,
            status: Some(crate::models::Status::Public),
            api_key: None,
            rcode_check_code: None,
        })
        .on_conflict(players::id)
        .do_update()
        .set((
            players::name.eq(new_game.name_a.clone()),
            players::platform.eq(new_game.platform_a),
        ))
        .execute(connection)
        .await
        .unwrap();

    insert_into(players::table)
        .values(&Player {
            id: new_game.id_b,
            name: new_game.name_b.clone(),
            platform: new_game.platform_b,
            status: Some(crate::models::Status::Public),
            api_key: None,
            rcode_check_code: None,
        })
        .on_conflict(players::id)
        .do_update()
        .set((
            players::name.eq(new_game.name_b.clone()),
            players::platform.eq(new_game.platform_b),
        ))
        .execute(connection)
        .await
        .unwrap();

    //Update player names in the player_names table
    insert_into(player_names::table)
        .values(&PlayerName {
            id: new_game.id_a,
            name: new_game.name_a.clone(),
        })
        .on_conflict_do_nothing()
        .execute(connection)
        .await
        .unwrap();

    insert_into(player_names::table)
        .values(&PlayerName {
            id: new_game.id_b,
            name: new_game.name_b.clone(),
        })
        .on_conflict_do_nothing()
        .execute(connection)
        .await
        .unwrap();

    Ok(())
}

async fn grab_games(connection: &mut AsyncPgConnection, redis_connection: &mut crate::RedisConnection<'_>) -> Result<Vec<Game>, String> {
    info!("Grabbing replays");

    let replays = ggst_api::get_replays().await;

    let mut replays = match replays {
        Ok(replays) => replays,
        Err(e) => {
            return Err(e);
        }
    };

    let num_replays = replays.len();
    info!("Got {num_replays} replays.");

    replays.reverse();

    let mut new_games = Vec::new();

    //Try to keep order if possible
    let mut seconds_offset = 0;

    for r in replays {
        let game_timestamp =
            NaiveDateTime::parse_from_str(&r.timestamp, "%Y-%m-%d %H:%M:%S").unwrap();

        let fixed_timestamp;

        // If the game_timestamp is more than 2 seconds in the future, set it to current time
        if game_timestamp > Utc::now().naive_utc()
            && game_timestamp > Utc::now().naive_utc() + chrono::Duration::seconds(2)
        {
            fixed_timestamp = Some(chrono::SubsecRound::round_subsecs(
                Utc::now().naive_utc() + chrono::Duration::seconds(seconds_offset),
                0,
            ));
        } else {
            fixed_timestamp = None;
        }

        let new_game = Game {
            timestamp: game_timestamp,
            real_timestamp: fixed_timestamp,
            id_a: r.player1.id.parse::<i64>().unwrap(),
            name_a: r.player1.name.clone(),
            char_a: i16::try_from(r.player1_character).ok().unwrap(),
            platform_a: i16::try_from(r.player1.platform).ok().unwrap(),
            id_b: r.player2.id.parse::<i64>().unwrap(),
            name_b: r.player2.name.clone(),
            char_b: i16::try_from(r.player2_character).ok().unwrap(),
            platform_b: i16::try_from(r.player2.platform).ok().unwrap(),
            winner: i16::try_from(r.winner).ok().unwrap(),
            game_floor: i16::try_from(r.floor).ok().unwrap(),
            value_a: None,
            deviation_a: None,
            value_b: None,
            deviation_b: None,
            win_chance: None,
        };

        if let Err(e) = update_player_info(connection, &new_game).await {
            error!("update_player_info failed: {e}");
        }

        let count = insert_into(games::table)
            .values(&new_game)
            .on_conflict_do_nothing()
            .execute(connection)
            .await
            .unwrap();

        if count > 0 {
            if fixed_timestamp.is_some() {
                seconds_offset += 1;
                debug!(
                    "Fixing timestamp {} -> {} - {} vs {}",
                    r.timestamp,
                    fixed_timestamp.unwrap(),
                    r.player1.name,
                    r.player2.name
                );
            }

            new_games.push(new_game);
        }
    }
    
    //Set set_latest_game_time for health check
    if let Some(last_game) = new_games.last() {
      let ts = last_game.real_timestamp.unwrap_or(last_game.timestamp);

        crate::imdb::set_latest_game_time(ts, redis_connection)
            .await
            .unwrap();
    }

    info!("Grabbing replays - Done");
    Ok(new_games)
}

async fn update_ratings(connection: &mut AsyncPgConnection) -> Result<(), String> {
    info!("Updating ratings");

    //Find all games for public players that are missing ratings
    let players_b = diesel::alias!(players as players_b);

    let new_games = games::table
        .inner_join(players::table.on(games::id_a.eq(players::id)))
        .inner_join(players_b.on(games::id_b.eq(players_b.field(players::id))))
        .select((
            games::all_columns,
            players::status,
            players_b.field(players::status),
        ))
        .filter(games::value_a.is_null())
        .order(coalesce(games::real_timestamp, games::timestamp).asc())
        .limit(5000)
        .get_results::<(Game, Option<Status>, Option<Status>)>(connection)
        .await
        .unwrap();

    info!("Found {} games without ratings", new_games.len());

    for entry in new_games {
        let g = entry.0;

        //Get the player_rating a
        let player_rating_a = match player_ratings::table
            .filter(player_ratings::id.eq(g.id_a))
            .filter(player_ratings::char_id.eq(g.char_a))
            .get_result::<PlayerRating>(connection)
            .await
        {
            Ok(r) => r,
            Err(_) => {
                //Insert the player rating if it's not found
                let new_player_rating = PlayerRating {
                    id: g.id_a,
                    char_id: g.char_a,
                    wins: 0,
                    losses: 0,
                    value: 1500.0,
                    deviation: 250.0,
                    last_decay: g.timestamp,
                };

                diesel::insert_into(player_ratings::table)
                    .values(&new_player_rating)
                    .execute(connection)
                    .await
                    .unwrap();

                new_player_rating
            }
        };
        //Get the player_rating b
        let player_rating_b = match player_ratings::table
            .filter(player_ratings::id.eq(g.id_b))
            .filter(player_ratings::char_id.eq(g.char_b))
            .get_result::<PlayerRating>(connection)
            .await
        {
            Ok(r) => r,
            Err(_) => {
                //Insert the player rating if it's not found
                let new_player_rating = PlayerRating {
                    id: g.id_b,
                    char_id: g.char_b,
                    wins: 0,
                    losses: 0,
                    value: 1500.0,
                    deviation: 250.0,
                    last_decay: g.timestamp,
                };

                diesel::insert_into(player_ratings::table)
                    .values(&new_player_rating)
                    .execute(connection)
                    .await
                    .unwrap();

                new_player_rating
            }
        };

        if Some(Status::Public) == entry.1 && Some(Status::Public) == entry.2 {
            //Calculate value and deviation
            let (new_value_a, new_deviation_a, new_value_b, new_deviation_b, win_prob) =
                crate::rating::update_mean_and_variance(
                    player_rating_a.value as f64,
                    player_rating_a.deviation as f64,
                    player_rating_b.value as f64,
                    player_rating_b.deviation as f64,
                    g.winner == 1,
                );

            //Update game table with player ratings
            diesel::update(games::table)
                .filter(games::timestamp.eq(g.timestamp))
                .filter(games::id_a.eq(g.id_a))
                .filter(games::char_a.eq(g.char_a))
                .filter(games::platform_a.eq(g.platform_a))
                .filter(games::id_b.eq(g.id_b))
                .filter(games::char_b.eq(g.char_b))
                .filter(games::platform_b.eq(g.platform_b))
                .set((
                    games::value_a.eq(player_rating_a.value),
                    games::deviation_a.eq(player_rating_a.deviation),
                    games::value_b.eq(player_rating_b.value),
                    games::deviation_b.eq(player_rating_b.deviation),
                    games::win_chance.eq(win_prob as f32),
                ))
                .execute(connection)
                .await
                .unwrap();

            //Update player_rating a
            diesel::update(player_ratings::table)
                .filter(player_ratings::id.eq(g.id_a))
                .filter(player_ratings::char_id.eq(g.char_a))
                .set((
                    player_ratings::value.eq(new_value_a as f32),
                    player_ratings::deviation.eq(new_deviation_a as f32),
                ))
                .execute(connection)
                .await
                .unwrap();

            //Update player_rating b
            diesel::update(player_ratings::table)
                .filter(player_ratings::id.eq(g.id_b))
                .filter(player_ratings::char_id.eq(g.char_b))
                .set((
                    player_ratings::value.eq(new_value_b as f32),
                    player_ratings::deviation.eq(new_deviation_b as f32),
                ))
                .execute(connection)
                .await
                .unwrap();
        } else {
            tracing::debug!("Hidden game: {} - {}", g.name_a, g.name_b);
            //Player is not public, so update game with 0s
            diesel::update(games::table)
                .filter(games::timestamp.eq(g.timestamp))
                .filter(games::id_a.eq(g.id_a))
                .filter(games::char_a.eq(g.char_a))
                .filter(games::platform_a.eq(g.platform_a))
                .filter(games::id_b.eq(g.id_b))
                .filter(games::char_b.eq(g.char_b))
                .filter(games::platform_b.eq(g.platform_b))
                .set((
                    games::value_a.eq(0.0),
                    games::deviation_a.eq(0.0),
                    games::value_b.eq(0.0),
                    games::deviation_b.eq(0.0),
                    games::win_chance.eq(0.0),
                ))
                .execute(connection)
                .await
                .unwrap();
        }
    }
    info!("Updating ratings - Done");
    Ok(())
}
