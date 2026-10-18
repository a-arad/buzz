-- Fence event_dispatch_queue like every other tenant table. A database that
-- applied 0058 on the 0.2.x line before 0029 was already fenced by 0029's
-- table loop, and attaching again is a no-op.
SELECT attach_community_write_fence('event_dispatch_queue');
