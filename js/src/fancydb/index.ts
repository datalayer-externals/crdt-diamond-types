import assert from 'assert/strict'
import Map2 from 'map2'
import { AtLeast1, CreateValue, DBValue, LV, Operation, Primitive, RawVersion, ROOT, ROOT_LV } from '../types'
import * as causalGraph from './causal-graph.js'
import { CausalGraph } from './causal-graph.js'

type RegisterValue = {type: 'primitive', val: Primitive}
  | {type: 'crdt', id: LV}

type MVRegister = AtLeast1<[LV, RegisterValue]>

type CRDTInfo = {
  type: 'map',
  registers: {[k: string]: MVRegister},
} | {
  type: 'set',
  values: Map<LV, RegisterValue>,
} | {
  type: 'register',
  value: MVRegister,
}

export interface FancyDB {
  crdts: Map<LV, CRDTInfo>,
  cg: CausalGraph,
}

export function createDb(): FancyDB {
  const db: FancyDB = {
    crdts: new Map(),
    cg: causalGraph.create(),
  }

  db.crdts.set(ROOT_LV, {
    type: "map",
    registers: {}
  })

  return db
}


function removeRecursive(db: FancyDB, value: RegisterValue) {
  if (value.type !== 'crdt') return

  const crdt = db.crdts.get(value.id)
  if (crdt == null) return

  switch (crdt.type) {
    case 'map':
      for (const k in crdt.registers) {
        const reg = crdt.registers[k]
        for (const [version, value] of reg) {
          removeRecursive(db, value)
        }
      }
      break
    case 'register':
      for (const [version, value] of crdt.value) {
        removeRecursive(db, value)
      }
      break
    case 'set':
      for (const [id, value] of crdt.values) {
        removeRecursive(db, value)
      }
      break
    default: throw Error('Unknown CRDT type!?')
  }

  db.crdts.delete(value.id)
}

const errExpr = (str: string): never => { throw Error(str) }

function createCRDT(db: FancyDB, id: LV, type: 'map' | 'set' | 'register') {
  if (db.crdts.has(id)) {
    throw Error('CRDT already exists !?')
  }

  const crdtInfo: CRDTInfo = type === 'map' ? {
    type: "map",
    registers: {},
  } : type === 'register' ? {
    type: 'register',
    // Registers default to NULL when created.
    value: [[id, {type: 'primitive', val: null}]],
  } : type === 'set' ? {
    type: 'set',
    values: new Map,
  } : errExpr('Invalid CRDT type')

  db.crdts.set(id, crdtInfo)
}

function mergeRegister(db: FancyDB, globalParents: LV[], oldPairs: MVRegister, localParents: LV[], newVersion: LV, newVal: CreateValue): MVRegister {
  let newValue: RegisterValue
  if (newVal.type === 'primitive') {
    newValue = newVal
  } else {
    // Create it.
    createCRDT(db, newVersion, newVal.crdtKind)
    newValue = {type: "crdt", id: newVersion}
  }

  const newPairs: MVRegister = [[newVersion, newValue]]
  for (const [version, value] of oldPairs) {
    // Each item is either retained or removed.
    if (localParents.some(v2 => version === v2)) {
      // The item was named in parents. Remove it.
      // console.log('removing', value)
      removeRecursive(db, value)
    } else {
      // We're intending to retain this operation because its not explicitly
      // named, but that only makes sense if the retained version is concurrent
      // with the new version.
      if (causalGraph.versionContainsTime(db.cg, globalParents, version)) {
        throw Error('Invalid local parents in operation')
      }

      newPairs.push([version, value])
    }
  }

  // Note we're sorting by *local version* here. This doesn't sort by LWW
  // priority. Could do - currently I'm figuring out the priority in the
  // get() method.
  newPairs.sort(([v1], [v2]) => v1 - v2)

  return newPairs
}

export function applyRemoteOp(db: FancyDB, op: Operation): LV {
  // if (causalGraph.tryRawToLV(db.cg, op.id[0], op.id[1]) != null) {
  //   // The operation is already known.
  //   console.warn('Operation already applied', op.id)
  //   return
  // }

  const newVersion = causalGraph.addRaw(db.cg, op.id, 1, op.globalParents)
  if (newVersion < 0) {
    // The operation is already known.
    console.warn('Operation already applied', op.id)
    return newVersion
  }

  const globalParents = causalGraph.rawToLVList(db.cg, op.globalParents)

  const crdtLV = causalGraph.rawToLV(db.cg, op.crdtId[0], op.crdtId[1])

  const crdt = db.crdts.get(crdtLV)
  if (crdt == null) {
    console.warn('CRDT has been deleted..')
    return newVersion
  }

  // Every register operation creates a new value, and removes 0-n other values.
  switch (op.action.type) {
    case 'registerSet': {
      if (crdt.type !== 'register') throw Error('Invalid operation type for target')
      const localParents = causalGraph.rawToLVList(db.cg, op.action.localParents)
      const newPairs = mergeRegister(db, globalParents, crdt.value, localParents, newVersion, op.action.val)

      crdt.value = newPairs
      break
    }
    case 'map': {
      if (crdt.type !== 'map') throw Error('Invalid operation type for target')

      const oldPairs = crdt.registers[op.action.key] ?? []
      const localParents = causalGraph.rawToLVList(db.cg, op.action.localParents)

      const newPairs = mergeRegister(db, globalParents, oldPairs, localParents, newVersion, op.action.val)

      crdt.registers[op.action.key] = newPairs
      break
    }
    case 'setInsert': case 'setDelete': { // Sets!
      if (crdt.type !== 'set') throw Error('Invalid operation type for target')

      // Set operations are comparatively much simpler, because insert
      // operations cannot be concurrent and multiple overlapping delete
      // operations are ignored.

      // throw Error('nyi')
      if (op.action.type == 'setInsert') {
        if (op.action.val.type === 'primitive') {
          crdt.values.set(newVersion, op.action.val)
        } else {
          createCRDT(db, newVersion, op.action.val.crdtKind)
          crdt.values.set(newVersion, {type: "crdt", id: newVersion})
        }
      } else {
        // Delete!
        const target = causalGraph.rawToLV(db.cg, op.action.target[0], op.action.target[1])
        let oldVal = crdt.values.get(target)
        if (oldVal != null) {
          removeRecursive(db, oldVal)
          crdt.values.delete(target)
        }
      }

      break
    }

    default: throw Error('Invalid action type')
  }

  return newVersion
}


export function localMapInsert(db: FancyDB, id: RawVersion, mapId: LV, key: string, val: CreateValue): [Operation, LV] {
  const crdt = db.crdts.get(mapId)
  if (crdt == null || crdt.type !== 'map') throw Error('Invalid CRDT')

  const crdtId = causalGraph.lvToRaw(db.cg, mapId)

  const localParentsLV = (crdt.registers[key] ?? []).map(([version]) => version)
  const localParents = causalGraph.lvToRawList(db.cg, localParentsLV)
  const op: Operation = {
    id,
    crdtId,
    globalParents: causalGraph.lvToRawList(db.cg, db.cg.version),
    action: { type: 'map', localParents, key, val }
  }

  // TODO: Could easily inline this - which would mean more code but higher performance.
  const v = applyRemoteOp(db, op)
  return [op, v]
}

const registerToVal = (db: FancyDB, r: RegisterValue): DBValue => (
  (r.type === 'primitive')
    ? r.val
    : get(db, r.id) // Recurse!
)

export function get(db: FancyDB, crdtId: LV = ROOT_LV): DBValue {
  const crdt = db.crdts.get(crdtId)
  if (crdt == null) { return null }

  switch (crdt.type) {
    case 'register': {
      // When there's a tie, the active value is based on the order in pairs.
      const activePair = causalGraph.tieBreakRegisters(db.cg, crdt.value)
      return registerToVal(db, activePair)
    }
    case 'map': {
      const result: {[k: string]: DBValue} = {}
      for (const k in crdt.registers) {
        const activePair = causalGraph.tieBreakRegisters(db.cg, crdt.registers[k])
        result[k] = registerToVal(db, activePair)
      }
      return result
    }
    case 'set': {
      const result = new Map2<string, number, DBValue>()

      for (const [version, value] of crdt.values) {
        const rawVersion = causalGraph.lvToRaw(db.cg, version)
        result.set(rawVersion[0], rawVersion[1], registerToVal(db, value))
      }

      return result
    }
    default: throw Error('Invalid CRDT type in DB')
  }
}

;(() => {

  let db = createDb()

  localMapInsert(db, ['seph', 0], ROOT_LV, 'yo', {type: 'primitive', val: 123})
  assert.deepEqual(get(db), {yo: 123})

  // ****
  db = createDb()
  // concurrent changes
  applyRemoteOp(db, {
    id: ['mike', 0],
    globalParents: [],
    crdtId: ROOT,
    action: {type: 'map', localParents: [], key: 'c', val: {type: 'primitive', val: 'mike'}},
  })
  applyRemoteOp(db, {
    id: ['seph', 1],
    globalParents: [],
    crdtId: ROOT,
    action: {type: 'map', localParents: [], key: 'c', val: {type: 'primitive', val: 'seph'}},
  })

  assert.deepEqual(get(db), {c: 'seph'})

  applyRemoteOp(db, {
    id: ['mike', 1],
    // globalParents: [['mike', 0]],
    globalParents: [['mike', 0], ['seph', 1]],
    crdtId: ROOT,
    // action: {type: 'map', localParents: [['mike', 0]], key: 'yo', val: {type: 'primitive', val: 'both'}},
    action: {type: 'map', localParents: [['mike', 0], ['seph', 1]], key: 'c', val: {type: 'primitive', val: 'both'}},
  })
  // console.dir(db, {depth: null})
  assert.deepEqual(get(db), {c: 'both'})

  // ****
  db = createDb()
  // Set a value in an inner map
  const [_, inner] = localMapInsert(db, ['seph', 1], ROOT_LV, 'stuff', {type: 'crdt', crdtKind: 'map'})
  localMapInsert(db, ['seph', 2], inner, 'cool', {type: 'primitive', val: 'definitely'})
  assert.deepEqual(get(db), {stuff: {cool: 'definitely'}})
  
  
  // // Insert a set
  // const innerSet = localMapInsert(db, ['seph', 2], ROOT, 'a set', {type: 'crdt', crdtKind: 'set'})
  // localSetInsert(db, ['seph', 3], innerSet.id, {type: 'primitive', val: 'whoa'})
  // localSetInsert(db, ['seph', 4], innerSet.id, {type: 'crdt', crdtKind: 'map'})
  
  // console.log('db', get(db))
  // console.log('db', db)
  
  
  // assert.deepEqual(db, fromJSON(toJSON(db)))
})()