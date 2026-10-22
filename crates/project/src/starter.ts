import { defineEdge, defineSchema, defineTable, v } from "simien:schema"

export default defineSchema({
  users: defineTable({
    name: v.string(),
    email: v.optional(v.string()),
  }),
  posts: defineTable({
    title: v.string(),
    body: v.string(),
  }),
  wrote: defineEdge("users", "posts"),
})
