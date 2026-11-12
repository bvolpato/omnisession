WITH summaries AS (
    SELECT 'antigravity-ide' AS provider,session_id,source_home,source_file,steps,summary FROM ide_step_declarations
    UNION ALL
    SELECT 'antigravity-desktop',session_id,source_home,source_file,steps,summary FROM desktop_step_declarations
), candidates AS (
    SELECT s.*,CASE
        WHEN p.steps=s.steps AND c.source_file_id=s.source_file AND c.source_contents_id=p.source_contents
            AND h.verified AND h.home=s.source_home THEN 2
        WHEN p.steps=s.steps THEN 1 ELSE 0 END AS priority
    FROM summaries s
    LEFT JOIN physical_step_declarations p USING(session_id,source_home,source_file)
    LEFT JOIN session_index c USING(provider,session_id)
    LEFT JOIN native_catalog_homes h USING(provider,session_id)
    WHERE s.summary IS NOT NULL
), ranked AS (
    SELECT *,max(priority) OVER (PARTITION BY session_id,source_home,source_file) AS best FROM candidates
)
INSERT OR IGNORE INTO physical_database_summaries
SELECT session_id,source_home,source_file,json_object(
    'title',CASE WHEN count(DISTINCT json_quote(summary->>'title'))=1
        THEN min(summary->>'title') END,
    'project_path',CASE WHEN count(DISTINCT json_quote(summary->>'project_path'))=1
        THEN min(summary->>'project_path') END,
    'git_branch',CASE WHEN count(DISTINCT json_quote(summary->>'git_branch'))=1
        THEN min(summary->>'git_branch') END,
    'created_at',CASE WHEN count(DISTINCT json_quote(summary->>'created_at'))=1
        THEN min(summary->>'created_at') END,
    'updated_at',CASE WHEN count(DISTINCT json_quote(summary->>'updated_at'))=1
        THEN min(summary->>'updated_at') END,
    'approximate',CASE WHEN count(DISTINCT json_quote(summary->>'approximate'))=1
        THEN coalesce(min(summary->>'approximate'),1) ELSE 1 END
) FROM ranked WHERE priority=best GROUP BY session_id,source_home,source_file;

UPDATE session_index AS c SET title=NULL,project_path=NULL,git_branch=NULL,created_at=NULL,
    updated_at=CASE WHEN updated_at_approximate THEN updated_at END
FROM native_catalog_homes AS h
WHERE c.provider='antigravity-desktop' AND h.provider=c.provider AND h.session_id=c.session_id AND h.verified
  AND c.source_file_id IS NOT NULL
  AND NOT EXISTS (SELECT 1 FROM physical_database_summaries actual
      WHERE actual.session_id=c.session_id AND actual.source_home=h.home AND actual.source_file=c.source_file_id)
  AND EXISTS (SELECT 1 FROM physical_database_summaries old
      WHERE old.session_id=c.session_id AND old.source_home=h.home AND old.source_file<>c.source_file_id
        AND (old.summary->>'title',old.summary->>'project_path',old.summary->>'git_branch') IS
            (c.title,c.project_path,c.git_branch));
