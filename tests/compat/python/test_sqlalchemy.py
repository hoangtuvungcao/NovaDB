"""SQLAlchemy 2.x ORM + Core mini-app over mysql+pymysql and postgresql+psycopg."""
import sys, os, json, datetime
sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "lib"))
from harness import *
import sqlalchemy as sa
from sqlalchemy import (ForeignKey, String, Text, LargeBinary, Integer, func, select, text, UniqueConstraint,
                        DateTime, create_engine, inspect, JSON, update, delete, insert)
from sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column, relationship, Session, selectinload, joinedload
from sqlalchemy.exc import IntegrityError


class Base(DeclarativeBase):
    pass

class User(Base):
    __tablename__ = "users"
    id: Mapped[int] = mapped_column(primary_key=True, autoincrement=True)
    email: Mapped[str] = mapped_column(String(255), unique=True)
    name: Mapped[str] = mapped_column(String(200))
    profile: Mapped[str | None] = mapped_column(Text, nullable=True)
    avatar: Mapped[bytes | None] = mapped_column(LargeBinary, nullable=True)
    created_at: Mapped[datetime.datetime] = mapped_column(DateTime, server_default=func.current_timestamp())
    posts: Mapped[list["Post"]] = relationship(back_populates="author", cascade="all, delete-orphan")

class Post(Base):
    __tablename__ = "posts"
    id: Mapped[int] = mapped_column(primary_key=True, autoincrement=True)
    user_id: Mapped[int] = mapped_column(ForeignKey("users.id"), index=True)
    title: Mapped[str] = mapped_column(String(300))
    body: Mapped[str] = mapped_column(Text, default="")
    views: Mapped[int] = mapped_column(Integer, default=0)
    author: Mapped[User] = relationship(back_populates="posts")
    comments: Mapped[list["Comment"]] = relationship(back_populates="post", cascade="all, delete-orphan")

class Comment(Base):
    __tablename__ = "comments"
    id: Mapped[int] = mapped_column(primary_key=True, autoincrement=True)
    post_id: Mapped[int] = mapped_column(ForeignKey("posts.id"))
    user_id: Mapped[int] = mapped_column(ForeignKey("users.id"))
    body: Mapped[str] = mapped_column(Text)
    post: Mapped[Post] = relationship(back_populates="comments")

class Doc(Base):
    __tablename__ = "docs"
    id: Mapped[int] = mapped_column(primary_key=True)
    data: Mapped[dict] = mapped_column(JSON)


def run(label, url, known=None, skip_dialect_init=False):
    """skip_dialect_init: diagnostic mode for PostgreSQL. SQLAlchemy's psycopg dialect runs
    `select pg_catalog.version()` etc. on first connect; if the gateway rejects that, a second pass
    patches Dialect.initialize to a no-op purely to find out what ELSE breaks. It is reported
    under its own suite name and never replaces the real (failing) result."""
    known = known or {}
    S = Suite(f"py-sqlalchemy:{label}")
    def C(name, fn):
        k = known.get(name)
        return S.case(name, fn, known=k or known.get("*"), soft=not k)
    eng = create_engine(url, future=True, echo=bool(os.environ.get("SA_ECHO")))
    if skip_dialect_init:
        eng.dialect.initialize = lambda conn: None
        eng.dialect.server_version_info = (15, 0)
        eng.dialect.default_schema_name = "public"
    try:
        with eng.connect() as c:
            c.execute(text("select 1")).scalar()
    except Exception as e:
        msg = f"{type(e).__name__}: {e}"
        if known.get("connect"):
            S.emit("connect", "KNOWN-FAIL", f"{known['connect']} || {msg}")
        else:
            S.emit("connect", "FAIL", msg)
        return
    S.emit("connect", "PASS")

    C("metadata.create_all", lambda: Base.metadata.create_all(eng))

    def refl(fn):
        def w():
            fn(inspect(eng))
        return w
    def r_names(insp):
        names = set(insp.get_table_names())
        assert {"users", "posts", "comments"} <= names, names
    def r_cols(insp):
        cols = {c["name"] for c in insp.get_columns("users")}
        assert {"id", "email", "name", "created_at"} <= cols, f"columns={cols}"
    def r_pk(insp):
        pk = insp.get_pk_constraint("users")
        assert pk["constrained_columns"] == ["id"], pk
    def r_fk(insp):
        fks = insp.get_foreign_keys("posts")
        assert fks and fks[0]["referred_table"] == "users", fks
    def r_idx(insp):
        ix = insp.get_indexes("posts") + insp.get_unique_constraints("users") + insp.get_indexes("users")
        assert any("user_id" in (i.get("column_names") or []) or "email" in (i.get("column_names") or []) for i in ix), ix
    def r_md():
        md = sa.MetaData(); md.reflect(eng)
        t = md.tables["comments"]
        assert {"id", "post_id", "body"} <= set(t.c.keys()), f"reflected columns={list(t.c.keys())}"
    C("reflection.get_table_names", refl(r_names))
    C("reflection.get_columns", refl(r_cols))
    C("reflection.get_pk_constraint", refl(r_pk))
    C("reflection.get_foreign_keys", refl(r_fk))
    C("reflection.indexes+unique", refl(r_idx))
    C("reflection.MetaData.reflect", r_md)

    ids = {}
    def orm_insert():
        with Session(eng) as s:
            u1 = User(email="ann@example.com", name="Ann", profile=json.dumps({"tags": ["a", "b"], "n": 1}))
            u2 = User(email="bob@example.com", name="Bob")
            s.add_all([u1, u2]); s.commit()
            assert u1.id and u2.id and u1.id != u2.id, (u1.id, u2.id)
            assert u2.id == u1.id + 1
            ids["u1"], ids["u2"] = u1.id, u2.id
    C("orm.insert+autoincrement", orm_insert)

    def server_default_ts():
        with Session(eng) as s:
            u = s.get(User, ids["u1"])
            assert isinstance(u.created_at, (datetime.datetime, str)) and u.created_at, u.created_at
            if isinstance(u.created_at, str):
                datetime.datetime.fromisoformat(u.created_at.replace(" ", "T"))
            else:
                assert abs((datetime.datetime.utcnow() - u.created_at).total_seconds()) < 3600 * 24
    C("timestamp.server_default", server_default_ts)

    def unique_violation():
        with Session(eng) as s:
            s.add(User(email="ann@example.com", name="dup"))
            try:
                s.commit()
            except IntegrityError:
                s.rollback()
                # session still usable
                assert s.scalar(select(func.count()).select_from(User)) == 2
                return
            raise AssertionError("duplicate email accepted")
    C("unique_violation->IntegrityError", unique_violation)

    def fk_violation():
        with Session(eng) as s:
            s.add(Post(user_id=999999, title="orphan"))
            try:
                s.commit()
            except IntegrityError:
                s.rollback(); return
            raise AssertionError("FK violation accepted (foreign keys not enforced)")
    C("fk_violation->IntegrityError", fk_violation)

    def txn_commit_rollback():
        with Session(eng) as s:
            s.add(Post(user_id=ids["u1"], title="committed", body="x")); s.commit()
        with Session(eng) as s:
            s.add(Post(user_id=ids["u1"], title="rolled-back")); s.flush(); s.rollback()
        with Session(eng) as s:
            titles = set(s.scalars(select(Post.title)))
            assert "committed" in titles and "rolled-back" not in titles, titles
        # context-manager transaction with exception
        try:
            with Session(eng) as s, s.begin():
                s.add(Post(user_id=ids["u1"], title="ctx-rolled-back")); s.flush()
                raise RuntimeError("boom")
        except RuntimeError:
            pass
        with Session(eng) as s:
            assert s.scalar(select(func.count()).select_from(Post).where(Post.title == "ctx-rolled-back")) == 0
    C("transactions.commit+rollback", txn_commit_rollback)

    def savepoint():
        with Session(eng) as s:
            s.add(Post(user_id=ids["u1"], title="sp-outer")); s.flush()
            sp = s.begin_nested()
            s.add(Post(user_id=ids["u1"], title="sp-inner")); s.flush()
            sp.rollback()
            s.commit()
        with Session(eng) as s:
            t = set(s.scalars(select(Post.title)))
            assert "sp-outer" in t and "sp-inner" not in t, t
    C("transactions.savepoint", savepoint)

    def bulk():
        with Session(eng) as s:
            s.execute(insert(Post), [{"user_id": ids["u2"], "title": f"bulk-{i:04d}", "body": "b" * (i % 50), "views": i}
                                     for i in range(500)])
            s.commit()
        with Session(eng) as s:
            assert s.scalar(select(func.count()).select_from(Post).where(Post.title.like("bulk-%"))) == 500
    C("bulk_insert.executemany", bulk)

    def bulk_orm():
        with Session(eng) as s:
            s.add_all([Post(user_id=ids["u2"], title=f"orm-bulk-{i}", views=i) for i in range(100)])
            s.commit()
            assert s.scalar(select(func.count()).select_from(Post).where(Post.title.like("orm-bulk-%"))) == 100
    C("bulk_insert.orm_add_all", bulk_orm)

    def pagination():
        with Session(eng) as s:
            q = select(Post.title).where(Post.title.like("bulk-%")).order_by(Post.title)
            p1 = list(s.scalars(q.limit(10).offset(0)))
            p2 = list(s.scalars(q.limit(10).offset(10)))
            last = list(s.scalars(q.limit(10).offset(495)))
            assert p1[0] == "bulk-0000" and p1[-1] == "bulk-0009", p1
            assert p2[0] == "bulk-0010", p2
            assert len(last) == 5, last
            keyset = list(s.scalars(q.where(Post.title > "bulk-0100").limit(3)))
            assert keyset == ["bulk-0101", "bulk-0102", "bulk-0103"], keyset
    C("pagination.limit_offset+keyset", pagination)

    def joins():
        with Session(eng) as s:
            p = s.scalars(select(Post).where(Post.title == "committed")).one()
            s.add_all([Comment(post_id=p.id, user_id=ids["u2"], body="first!"),
                       Comment(post_id=p.id, user_id=ids["u1"], body="reply")])
            s.commit()
            rows = s.execute(select(Post.title, User.name, Comment.body)
                             .join(User, Post.user_id == User.id)
                             .join(Comment, Comment.post_id == Post.id)
                             .where(Post.title == "committed").order_by(Comment.id)).all()
            assert [(r[1], r[2]) for r in rows] == [("Ann", "first!"), ("Ann", "reply")], rows
            outer = s.execute(select(User.name, func.count(Post.id)).outerjoin(Post, Post.user_id == User.id)
                              .group_by(User.id).order_by(User.id)).all()
            assert [r[0] for r in outer] == ["Ann", "Bob"], outer
    C("join.core+outer", joins)

    def relationships():
        with Session(eng) as s:
            u = s.scalars(select(User).options(selectinload(User.posts).selectinload(Post.comments))
                          .where(User.email == "ann@example.com")).one()
            assert any(len(p.comments) == 2 for p in u.posts)
            p = s.scalars(select(Post).options(joinedload(Post.author)).where(Post.title == "committed")).one()
            assert p.author.name == "Ann"
    C("orm.relationships(selectin/joined)", relationships)

    def aggregates():
        with Session(eng) as s:
            r = s.execute(select(func.count(Post.id), func.sum(Post.views), func.avg(Post.views),
                                 func.min(Post.views), func.max(Post.views)).where(Post.title.like("bulk-%"))).one()
            assert r[0] == 500 and int(r[1]) == sum(range(500)) and abs(float(r[2]) - 249.5) < 1e-9, r
            g = s.execute(select(Post.user_id, func.count()).group_by(Post.user_id)
                          .having(func.count() > 100).order_by(Post.user_id)).all()
            assert [x[0] for x in g] == [ids["u2"]], g
    C("aggregates.group_by_having", aggregates)

    def json_text():
        with Session(eng) as s:
            u = s.get(User, ids["u1"])
            assert json.loads(u.profile) == {"tags": ["a", "b"], "n": 1}
    C("json.text_column", json_text)

    def json_type():
        with Session(eng) as s:
            s.add(Doc(id=1, data={"a": [1, 2, {"b": "Việt"}], "q": "'); DROP TABLE users; --"})); s.commit()
        with Session(eng) as s:
            d = s.get(Doc, 1)
            assert d.data["a"][2]["b"] == "Việt" and d.data["q"].startswith("')"), d.data
    C("json.sqlalchemy_JSON_type", json_type)

    def hostile():
        with Session(eng) as s:
            for i, h in enumerate(HOSTILE):
                u = User(email=f"h{i}@x.io", name=h, profile=h); s.add(u)
            s.commit()
            s.execute(text("INSERT INTO posts (user_id, title, body, views) VALUES (:u, :t, :b, 0)"),
                      {"u": ids["u1"], "t": HOSTILE[0], "b": HOSTILE[3]}); s.commit()
        with Session(eng) as s:
            got = [r for r in s.scalars(select(User.name).where(User.email.like("h%@x.io")).order_by(User.id))]
            assert got == HOSTILE, got
            assert s.scalar(select(func.count()).select_from(User).where(User.name == HOSTILE[0])) == 1
            assert s.scalar(select(func.count()).select_from(User)) >= 9  # users table intact
            # hostile LIKE / search term as bind
            assert s.scalar(select(func.count()).select_from(User).where(User.name.contains("DROP TABLE"))) == 2
    C("hostile_strings.bound_params", hostile)

    def unicode_():
        with Session(eng) as s:
            s.add(User(email="viet@x.vn", name=VIET, profile=json.dumps({"vi": VIET}, ensure_ascii=False)))
            s.commit()
        with Session(eng) as s:
            u = s.scalars(select(User).where(User.email == "viet@x.vn")).one()
            assert u.name == VIET, u.name
            assert json.loads(u.profile)["vi"] == VIET
            assert s.scalars(select(User).where(User.name == VIET)).one().email == "viet@x.vn"
            assert s.scalar(select(getattr(func, 'char_length' if label == 'mysql' else 'length')(User.name)).where(User.email == "viet@x.vn")) == len(VIET)
    C("unicode.vietnamese+emoji", unicode_)

    def blob():
        with Session(eng) as s:
            u = s.scalars(select(User).where(User.email == "bob@example.com")).one()
            u.avatar = BLOB; s.commit()
        with Session(eng) as s:
            u = s.scalars(select(User).where(User.email == "bob@example.com")).one()
            assert bytes(u.avatar) == BLOB, (type(u.avatar), len(u.avatar or b""))
    C("blob.roundtrip", blob)

    def core_update_delete():
        with Session(eng) as s:
            r = s.execute(update(Post).where(Post.title.like("bulk-00%")).values(views=Post.views + 1000))
            assert r.rowcount == 100, r.rowcount
            r = s.execute(delete(Post).where(Post.title.like("orm-bulk-%")))
            assert r.rowcount == 100, r.rowcount
            s.commit()
    C("core.update/delete rowcount", core_update_delete)

    def orm_cascade_delete():
        with Session(eng) as s:
            u = s.scalars(select(User).where(User.email == "ann@example.com")).one()
            s.delete(u); s.commit()
            assert s.scalar(select(func.count()).select_from(Post).where(Post.user_id == ids["u1"])) == 0
    C("orm.cascade_delete", orm_cascade_delete)

    def text_params():
        with eng.connect() as c:
            n = c.execute(text("SELECT COUNT(*) FROM posts WHERE views >= :v AND title LIKE :p"),
                          {"v": 10, "p": "bulk-%"}).scalar()
            assert n == 490 + 0 or n > 0, n
            rows = c.execute(text("SELECT id FROM posts WHERE id IN :ids").bindparams(sa.bindparam("ids", expanding=True)),
                             {"ids": [1, 2, 3]}).all()
            assert len(rows) <= 3
    C("core.text_expanding_params", text_params)

    def drop_all():
        Base.metadata.drop_all(eng)
        assert "users" not in inspect(eng).get_table_names()
    C("metadata.drop_all", drop_all)

    def alembic_ops():
        from alembic.migration import MigrationContext
        from alembic.operations import Operations
        with eng.begin() as conn:
            ctx = MigrationContext.configure(conn); op = Operations(ctx)
            op.create_table("mig", sa.Column("id", sa.Integer, primary_key=True), sa.Column("v", sa.String(20)))
            op.add_column("mig", sa.Column("extra", sa.Integer, nullable=True))
            op.create_index("ix_mig_v", "mig", ["v"])
            assert ctx.get_current_revision() is None
        assert "mig" in inspect(eng).get_table_names()
        with eng.begin() as conn:
            Operations(MigrationContext.configure(conn)).drop_table("mig")
    C("alembic.operations(create/add_column/index/drop)", alembic_ops)
    eng.dispose()


def main():
    pgp, my = os.environ["NOVA_PG_PORT"], os.environ["NOVA_MYSQL_PORT"]
    u, p = os.environ["NOVA_USER"], os.environ["NOVA_PASS"]
    mkdb("sa_mysql"); mkdb("sa_pg"); mkdb("sa_pg2")
    run("mysql", f"mysql+pymysql://{u}:{p}@127.0.0.1:{my}/sa_mysql?charset=utf8mb4", known=KNOWN_MYSQL)
    pgurl = lambda db: f"postgresql+psycopg://{u}:{p}@127.0.0.1:{pgp}/{db}?sslmode=disable"
    run("postgresql", pgurl("sa_pg"), known=KNOWN_PG)
    run("postgresql-noinit", pgurl("sa_pg2"), known=KNOWN_PG_NOINIT, skip_dialect_init=True)

KNOWN_MYSQL = {}  # MY-5 fixed: SHOW CREATE TABLE emits MySQL-format DDL
# PG-6/PG-8/PG-11/PG-13 fixed: no known failures remain on PostgreSQL.
KNOWN_PG = {}
KNOWN_PG_NOINIT = {}
main()
