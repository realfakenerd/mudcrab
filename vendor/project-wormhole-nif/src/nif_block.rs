use project_wormhole_ba2::dev::{Bool, Bounds, MaxRef};
use project_wormhole_shared::glam::{self, Vec3};

use super::dev::*;

pub enum NifBlock {
    BSBehaviorGraphExtraData(BSBehaviorGraphExtraData),
    BSBlastNode(BSBlastNode),
    BSBoneLODExtraData(BSBoneLODExtraData),
    BSBound(BSBound),
    BSClothExtraData(BSClothExtraData),
    BSConnectPointChildren(BSConnectPointChildren),
    BSConnectPointParents(BSConnectPointParents),
    BSDamageStage(BSDamageStage),
    BSDynamicTriShape(BSDynamicTriShape),
    BSEffectShaderProperty(BSEffectShaderProperty),
    BSEffectShaderPropertyColorController(BSEffectShaderPropertyColorController),
    BSEffectShaderPropertyFloatController(BSEffectShaderPropertyFloatController),
    BSEyeCenterExtraData(BSEyeCenterExtraData),
    BSFadeNode(NiNode),
    BSFrustumFOVController(BSFrustumFOVController),
    BSFurnitureMarkerNode(BSFurnitureMarkerNode),
    BSLagBoneController(BSLagBoneController),
    BSLeafAnimNode(BSLeafAnimNode),
    BSLODTriShape(BSLODTriShape),
    BSLightingShaderProperty(BSLightingShaderProperty),
    BSLightingShaderPropertyColorController(BSLightingShaderPropertyColorController),
    BSLightingShaderPropertyFloatController(BSLightingShaderPropertyFloatController),
    BSMasterParticleSystem(BSMasterParticleSystem),
    BSMeshLODTriShape(BSMeshLODTriShape),
    BSMultiBound(BSMultiBound),
    BSMultiBoundAABB(BSMultiBoundAABB),
    BSMultiBoundNode(BSMultiBoundNode),
    BSMultiBoundOBB(BSMultiBoundOBB),
    BSNiAlphaPropertyTestRefController(BSNiAlphaPropertyTestRefController),
    BSOrderedNode(BSOrderedNode),
    BSPSysInheritVelocityModifier(BSPSysInheritVelocityModifier),
    BSPSysLODModifier(BSPSysLODModifier),
    BSPSysMultiTargetEmitterCtlr(BSPSysMultiTargetEmitterCtlr),
    BSPSysRecycleBoundModifier(BSPSysRecycleBoundModifier),
    BSPSysScaleModifier(BSPSysScaleModifier),
    BSPSysSimpleColorModifier(BSPSysSimpleColorModifier),
    BSPSysSubTexModifier(BSPSysSubTexModifier),
    BSParentVelocityModifier(BSParentVelocityModifier),
    BSPositionData(BSPositionData),
    BSProceduralLightningController(BSProceduralLightningController),
    BSShaderTextureSet(BSShaderTextureSet),
    BSSkinBoneData(BSSkinBoneData),
    BSSkinInstance(BSSkinInstance),
    BSSkyShaderProperty(BSSkyShaderProperty),
    BSSubIndexTriShape(BSSubIndexTriShape),
    BSTreeNode(BSTreeNode),
    BSTriShape(BSTriShape),
    BSValueNode(BSValueNode),
    BSWaterShaderProperty(BSWaterShaderProperty),
    BSWindModifier(BSWindModifier),
    BSXFlags(BSXFlags),
    NiAlphaProperty(NiAlphaProperty),
    NiBillboardNode(NiBillboardNode),
    NiBinaryExtraData(NiBinaryExtraData),
    NiBlendBoolInterpolator(NiBlendBoolInterpolator),
    NiBlendFloatInterpolator(NiBlendFloatInterpolator),
    NiBlendPoint3Interpolator(NiBlendPoint3Interpolator),
    NiBoolData(NiBoolData),
    NiBoolInterpolator(NiBoolInterpolator),
    NiBoolTimelineInterpolator(NiBoolTimelineInterpolator),
    NiCamera(NiCamera),
    NiControllerManager(NiControllerManager),
    NiControllerSequence(NiControllerSequence),
    NiDefaultAVObjectPalette(NiDefaultAVObjectPalette),
    NiExtraData(NiExtraData),
    NiFloatData(NiFloatData),
    NiFloatInterpolator(NiFloatInterpolator),
    NiLightColorController(NiLightColorController),
    NiLightDimmerController(NiLightDimmerController),
    NiLightRadiusController(NiLightRadiusController),
    NiMultiTargetTransformController(NiMultiTargetTransformController),
    NiNode(NiNode),
    NiPSysAgeDeathModifier(NiPSysAgeDeathModifier),
    NiPSysBombModifier(NiPSysBombModifier),
    NiPSysBoundUpdateModifier(NiPSysBoundUpdateModifier),
    NiPSysBoxEmitter(NiPSysBoxEmitter),
    NiPSysColliderManager(NiPSysColliderManager),
    NiPSysCylinderEmitter(NiPSysCylinderEmitter),
    NiPSysData(NiPSysData),
    NiPSysDragModifier(NiPSysDragModifier),
    NiPSysEmitterCtlr(NiPSysEmitterCtlr),
    NiPSysEmitterDeclinationCtlr(NiPSysEmitterDeclinationCtlr),
    NiPSysEmitterDeclinationVarCtlr(NiPSysEmitterDeclinationVarCtlr),
    NiPSysEmitterInitialRadiusCtlr(NiPSysEmitterInitialRadiusCtlr),
    NiPSysEmitterLifeSpanCtlr(NiPSysEmitterLifeSpanCtlr),
    NiPSysEmitterPlanarAngleCtlr(NiPSysEmitterPlanarAngleCtlr),
    NiPSysEmitterSpeedCtlr(NiPSysEmitterSpeedCtlr),
    NiPSysGravityModifier(NiPSysGravityModifier),
    NiPSysGravityStrengthCtlr(NiPSysGravityStrengthCtlr),
    NiPSysInitialRotSpeedCtlr(NiPSysInitialRotSpeedCtlr),
    NiPSysInitialRotSpeedVarCtlr(NiPSysInitialRotSpeedVarCtlr),
    NiPSysMeshEmitter(NiPSysMeshEmitter),
    NiPSysModifierActiveCtlr(NiPSysModifierActiveCtlr),
    NiPSysPlanarCollider(NiPSysPlanarCollider),
    NiPSysPositionModifier(NiPSysPositionModifier),
    NiPSysRotationModifier(NiPSysRotationModifier),
    NiPSysSpawnModifier(NiPSysSpawnModifier),
    NiPSysSphereEmitter(NiPSysSphereEmitter),
    NiPSysSphericalCollider(NiPSysSphericalCollider),
    NiPSysUpdateCtlr(NiPSysUpdateCtlr),
    NiParticleSystem(NiParticleSystem),
    NiPathInterpolator(NiPathInterpolator),
    NiPoint3Interpolator(NiPoint3Interpolator),
    NiPointLight(NiPointLight),
    NiPosData(NiPosData),
    NiSkinInstance(NiSkinInstance),
    NiSkinPartition(NiSkinPartition),
    NiStringExtraData(NiStringExtraData),
    NiStringsExtraData(NiStringsExtraData),
    NiSwitchNode(NiSwitchNode),
    NiTextKeyExtraData(NiTextKeyExtraData),
    NiTriShape(NiTriShape),
    NiTriShapeData(NiTriShapeData),
    NiTransformController(NiTransformController),
    NiTransformData(NiTransformData),
    NiTransformInterpolator(NiTransformInterpolator),
    NiVisController(NiVisController),
    BHKNPCollisionObject(BHKNPCollisionObject),
    BHKPhysicsSystem(BHKPhysicsSystem),
    BHKRagdollSystem(BHKRagdollSystem),
    Unhandled,
}
impl std::fmt::Debug for NifBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NifBlock::BSBehaviorGraphExtraData(_) => write!(f, "BSBehaviorGraphExtraData"),
            NifBlock::BSBlastNode(_) => write!(f, "BSBlastNode"),
            NifBlock::BSBoneLODExtraData(_) => write!(f, "BSBoneLODExtraData"),
            NifBlock::BSBound(_) => write!(f, "BSBound"),
            NifBlock::BSClothExtraData(_) => write!(f, "BSClothExtraData"),
            NifBlock::BSConnectPointChildren(_) => write!(f, "BSConnectPointChildren"),
            NifBlock::BSConnectPointParents(_) => write!(f, "BSConnectPointParents"),
            NifBlock::BSDamageStage(_) => write!(f, "BSDamageStage"),
            NifBlock::BSDynamicTriShape(_) => write!(f, "BSDynamicTriShape"),
            NifBlock::BSEffectShaderProperty(_) => write!(f, "BSEffectShaderProperty"),
            NifBlock::BSEffectShaderPropertyColorController(_) => {
                write!(f, "BSEffectShaderPropertyColorController")
            }
            NifBlock::BSEffectShaderPropertyFloatController(_) => {
                write!(f, "BSEffectShaderPropertyFloatController")
            }
            NifBlock::BSEyeCenterExtraData(_) => write!(f, "BSEyeCenterExtraData"),
            NifBlock::BSFadeNode(_) => write!(f, "BSFadeNode"),
            NifBlock::BSFrustumFOVController(_) => write!(f, "BSFrustumFOVController"),
            NifBlock::BSFurnitureMarkerNode(_) => write!(f, "BSFurnitureMarkerNode"),
            NifBlock::BSLagBoneController(_) => write!(f, "BSLagBoneController"),
            NifBlock::BSLeafAnimNode(_) => write!(f, "BSLeafAnimNode"),
            NifBlock::BSLODTriShape(_) => write!(f, "BSLODTriShape"),
            NifBlock::BSLightingShaderProperty(_) => write!(f, "BSLightingShaderProperty"),
            NifBlock::BSLightingShaderPropertyColorController(_) => {
                write!(f, "BSLightingShaderPropertyColorController")
            }
            NifBlock::BSLightingShaderPropertyFloatController(_) => {
                write!(f, "BSLightingShaderPropertyFloatController")
            }
            NifBlock::BSMasterParticleSystem(_) => write!(f, "BSMasterParticleSystem"),
            NifBlock::BSMeshLODTriShape(_) => write!(f, "BSMeshLODTriShape"),
            NifBlock::BSMultiBound(_) => write!(f, "BSMultiBound"),
            NifBlock::BSMultiBoundAABB(_) => write!(f, "BSMultiBoundAABB"),
            NifBlock::BSMultiBoundNode(_) => write!(f, "BSMultiBoundNode"),
            NifBlock::BSMultiBoundOBB(_) => write!(f, "BSMultiBoundOBB"),
            NifBlock::BSNiAlphaPropertyTestRefController(_) => {
                write!(f, "BSNiAlphaPropertyTestRefController")
            }
            NifBlock::BSOrderedNode(_) => write!(f, "BSOrderedNode"),
            NifBlock::BSPSysInheritVelocityModifier(_) => {
                write!(f, "BSPSysInheritVelocityModifier")
            }
            NifBlock::BSPSysLODModifier(_) => write!(f, "BSPSysLODModifier"),
            NifBlock::BSPSysMultiTargetEmitterCtlr(_) => write!(f, "BSPSysMultiTargetEmitterCtlr"),
            NifBlock::BSPSysRecycleBoundModifier(_) => write!(f, "BSPSysRecycleBoundModifier"),
            NifBlock::BSPSysScaleModifier(_) => write!(f, "BSPSysScaleModifier"),
            NifBlock::BSPSysSimpleColorModifier(_) => write!(f, "BSPSysSimpleColorModifier"),
            NifBlock::BSPSysSubTexModifier(_) => write!(f, "BSPSysSubTexModifier"),
            NifBlock::BSParentVelocityModifier(_) => write!(f, "BSParentVelocityModifier"),
            NifBlock::BSPositionData(_) => write!(f, "BSPositionData"),
            NifBlock::BSProceduralLightningController(_) => {
                write!(f, "BSProceduralLightningController")
            }
            NifBlock::BSShaderTextureSet(_) => write!(f, "BSShaderTextureSet"),
            NifBlock::BSSkinBoneData(_) => write!(f, "BSSkinBoneData"),
            NifBlock::BSSkinInstance(_) => write!(f, "BSSkinInstance"),
            NifBlock::BSSkyShaderProperty(_) => write!(f, "BSSkyShaderProperty"),
            NifBlock::BSSubIndexTriShape(_) => write!(f, "BSSubIndexTriShape"),
            NifBlock::BSTreeNode(_) => write!(f, "BSTreeNode"),
            NifBlock::BSTriShape(_) => write!(f, "BSTriShape"),
            NifBlock::BSValueNode(_) => write!(f, "BSValueNode"),
            NifBlock::BSWaterShaderProperty(_) => write!(f, "BSWaterShaderProperty"),
            NifBlock::BSWindModifier(_) => write!(f, "BSWindModifier"),
            NifBlock::BSXFlags(_) => write!(f, "BSXFlags"),
            NifBlock::NiAlphaProperty(_) => write!(f, "NiAlphaProperty"),
            NifBlock::NiBillboardNode(_) => write!(f, "NiBillboardNode"),
            NifBlock::NiBinaryExtraData(_) => write!(f, "NiBinaryExtraData"),
            NifBlock::NiBlendBoolInterpolator(_) => write!(f, "NiBlendBoolInterpolator"),
            NifBlock::NiBlendFloatInterpolator(_) => write!(f, "NiBlendFloatInterpolator"),
            NifBlock::NiBlendPoint3Interpolator(_) => write!(f, "NiBlendPoint3Interpolator"),
            NifBlock::NiBoolData(_) => write!(f, "NiBoolData"),
            NifBlock::NiBoolInterpolator(_) => write!(f, "NiBoolInterpolator"),
            NifBlock::NiBoolTimelineInterpolator(_) => write!(f, "NiBoolTimelineInterpolator"),
            NifBlock::NiCamera(_) => write!(f, "NiCamera"),
            NifBlock::NiControllerManager(_) => write!(f, "NiControllerManager"),
            NifBlock::NiControllerSequence(_) => write!(f, "NiControllerSequence"),
            NifBlock::NiDefaultAVObjectPalette(_) => write!(f, "NiDefaultAVObjectPalette"),
            NifBlock::NiExtraData(_) => write!(f, "NiExtraData"),
            NifBlock::NiFloatData(_) => write!(f, "NiFloatData"),
            NifBlock::NiFloatInterpolator(_) => write!(f, "NiFloatInterpolator"),
            NifBlock::NiLightColorController(_) => write!(f, "NiLightColorController"),
            NifBlock::NiLightDimmerController(_) => write!(f, "NiLightDimmerController"),
            NifBlock::NiLightRadiusController(_) => write!(f, "NiLightRadiusController"),
            NifBlock::NiMultiTargetTransformController(_) => {
                write!(f, "NiMultiTargetTransformController")
            }
            NifBlock::NiNode(_) => write!(f, "NiNode"),
            NifBlock::NiPSysAgeDeathModifier(_) => write!(f, "NiPSysAgeDeathModifier"),
            NifBlock::NiPSysBombModifier(_) => write!(f, "NiPSysBombModifier"),
            NifBlock::NiPSysBoundUpdateModifier(_) => write!(f, "NiPSysBoundUpdateModifier"),
            NifBlock::NiPSysBoxEmitter(_) => write!(f, "NiPSysBoxEmitter"),
            NifBlock::NiPSysColliderManager(_) => write!(f, "NiPSysColliderManager"),
            NifBlock::NiPSysCylinderEmitter(_) => write!(f, "NiPSysCylinderEmitter"),
            NifBlock::NiPSysData(_) => write!(f, "NiPSysData"),
            NifBlock::NiPSysDragModifier(_) => write!(f, "NiPSysDragModifier"),
            NifBlock::NiPSysEmitterCtlr(_) => write!(f, "NiPSysEmitterCtlr"),
            NifBlock::NiPSysEmitterDeclinationCtlr(_) => write!(f, "NiPSysEmitterDeclinationCtlr"),
            NifBlock::NiPSysEmitterDeclinationVarCtlr(_) => {
                write!(f, "NiPSysEmitterDeclinationVarCtlr")
            }
            NifBlock::NiPSysEmitterInitialRadiusCtlr(_) => {
                write!(f, "NiPSysEmitterInitialRadiusCtlr")
            }
            NifBlock::NiPSysEmitterLifeSpanCtlr(_) => write!(f, "NiPSysEmitterLifeSpanCtlr"),
            NifBlock::NiPSysEmitterPlanarAngleCtlr(_) => write!(f, "NiPSysEmitterPlanarAngleCtlr"),
            NifBlock::NiPSysEmitterSpeedCtlr(_) => write!(f, "NiPSysEmitterSpeedCtlr"),
            NifBlock::NiPSysGravityModifier(_) => write!(f, "NiPSysGravityModifier"),
            NifBlock::NiPSysGravityStrengthCtlr(_) => write!(f, "NiPSysGravityStrengthCtlr"),
            NifBlock::NiPSysInitialRotSpeedCtlr(_) => write!(f, "NiPSysInitialRotSpeedCtlr"),
            NifBlock::NiPSysInitialRotSpeedVarCtlr(_) => write!(f, "NiPSysInitialRotSpeedVarCtlr"),
            NifBlock::NiPSysMeshEmitter(_) => write!(f, "NiPSysMeshEmitter"),
            NifBlock::NiPSysModifierActiveCtlr(_) => write!(f, "NiPSysModifierActiveCtlr"),
            NifBlock::NiPSysPlanarCollider(_) => write!(f, "NiPSysPlanarCollider"),
            NifBlock::NiPSysPositionModifier(_) => write!(f, "NiPSysPositionModifier"),
            NifBlock::NiPSysRotationModifier(_) => write!(f, "NiPSysRotationModifier"),
            NifBlock::NiPSysSpawnModifier(_) => write!(f, "NiPSysSpawnModifier"),
            NifBlock::NiPSysSphereEmitter(_) => write!(f, "NiPSysSphereEmitter"),
            NifBlock::NiPSysSphericalCollider(_) => write!(f, "NiPSysSphericalCollider"),
            NifBlock::NiPSysUpdateCtlr(_) => write!(f, "NiPSysUpdateCtlr"),
            NifBlock::NiParticleSystem(_) => write!(f, "NiParticleSystem"),
            NifBlock::NiPathInterpolator(_) => write!(f, "NiPathInterpolator"),
            NifBlock::NiPoint3Interpolator(_) => write!(f, "NiPoint3Interpolator"),
            NifBlock::NiPointLight(_) => write!(f, "NiPointLight"),
            NifBlock::NiPosData(_) => write!(f, "NiPosData"),
            NifBlock::NiSkinInstance(_) => write!(f, "NiSkinInstance"),
            NifBlock::NiSkinPartition(_) => write!(f, "NiSkinPartition"),
            NifBlock::NiStringExtraData(_) => write!(f, "NiStringExtraData"),
            NifBlock::NiStringsExtraData(_) => write!(f, "NiStringsExtraData"),
            NifBlock::NiSwitchNode(_) => write!(f, "NiSwitchNode"),
            NifBlock::NiTextKeyExtraData(_) => write!(f, "NiTextKeyExtraData"),
            NifBlock::NiTriShape(_) => write!(f, "NiTriShape"),
            NifBlock::NiTriShapeData(_) => write!(f, "NiTriShapeData"),
            NifBlock::NiTransformController(_) => write!(f, "NiTransformController"),
            NifBlock::NiTransformData(_) => write!(f, "NiTransformData"),
            NifBlock::NiTransformInterpolator(_) => write!(f, "NiTransformInterpolator"),
            NifBlock::NiVisController(_) => write!(f, "NiVisController"),
            NifBlock::BHKNPCollisionObject(_) => write!(f, "BHKNPCollisionObject"),
            NifBlock::BHKPhysicsSystem(_) => write!(f, "BHKPhysicsSystem"),
            NifBlock::BHKRagdollSystem(_) => write!(f, "BHKRagdollSystem"),
            NifBlock::Unhandled => write!(f, "Unhandled"),
        }
    }
}

/// Drops the zero word a distant-LOD shape carries after its geometry.
///
/// The `BSTriShape` blocks of Skyrim's terrain LOD meshes (`meshes/terrain/**`
/// `.btr`) are exactly their geometry payload plus one zero `u32`, which the
/// payload itself does not account for, while ordinary shapes end at the
/// payload. Anything else is left in place for the leftover warning.
fn take_lod_shape_trailing_word(i: &[u8]) -> &[u8] {
    if i.len() == 4 && i.iter().all(|byte| *byte == 0) {
        &i[4..]
    } else {
        i
    }
}

impl NifBlock {
    pub fn parse(i: &[u8], block_type: String) -> IResult<&[u8], Self> {
        match block_type.as_str() {
            "NiNode" => {
                let (i, result) = NiNode::parse(i)?;
                if i.len() > 0 {
                    warn!("{} bytes left over after parsing NiNode", i.len());
                }
                Ok((i, NifBlock::NiNode(result)))
            }

            "BSFadeNode" => {
                let (i, result) = NiNode::parse(i)?;
                if !i.is_empty() {
                    warn!("{} bytes left over after parsing BSFadeNode", i.len());
                }
                Ok((i, NifBlock::BSFadeNode(result)))
            }

            "BSTriShape" => {
                let (i, result) = BSTriShape::parse(i)?;
                let i = take_lod_shape_trailing_word(i);
                if i.len() > 0 {
                    warn!("{} bytes left over after parsing BSTriShape", i.len());
                }
                Ok((i, NifBlock::BSTriShape(result)))
            }

            "BSDynamicTriShape" => {
                let (i, result) = BSDynamicTriShape::parse(i)?;
                if !i.is_empty() {
                    warn!(
                        "{} bytes left over after parsing BSDynamicTriShape",
                        i.len()
                    );
                }
                Ok((i, NifBlock::BSDynamicTriShape(result)))
            }

            "BSSubIndexTriShape" => {
                let (i, result) = BSSubIndexTriShape::parse(i)?;
                if i.len() > 0 {
                    warn!(
                        "{} bytes left over after parsing BSSubIndexTriShape",
                        i.len()
                    );
                }
                Ok((i, NifBlock::BSSubIndexTriShape(result)))
            }

            "BSLODTriShape" => {
                let (i, result) = BSLODTriShape::parse(i)?;
                if !i.is_empty() {
                    warn!("{} bytes left over after parsing BSLODTriShape", i.len());
                }
                Ok((i, NifBlock::BSLODTriShape(result)))
            }

            "BSMultiBoundNode" => {
                let (i, result) = BSMultiBoundNode::parse(i)?;
                if !i.is_empty() {
                    warn!("{} bytes left over after parsing BSMultiBoundNode", i.len());
                }
                Ok((i, NifBlock::BSMultiBoundNode(result)))
            }

            "BSMultiBound" => {
                let (i, result) = BSMultiBound::parse(i)?;
                if !i.is_empty() {
                    warn!("{} bytes left over after parsing BSMultiBound", i.len());
                }
                Ok((i, NifBlock::BSMultiBound(result)))
            }

            "BSMultiBoundAABB" => {
                let (i, result) = BSMultiBoundAABB::parse(i)?;
                if !i.is_empty() {
                    warn!("{} bytes left over after parsing BSMultiBoundAABB", i.len());
                }
                Ok((i, NifBlock::BSMultiBoundAABB(result)))
            }

            "BSMultiBoundOBB" => {
                let (i, result) = BSMultiBoundOBB::parse(i)?;
                if !i.is_empty() {
                    warn!("{} bytes left over after parsing BSMultiBoundOBB", i.len());
                }
                Ok((i, NifBlock::BSMultiBoundOBB(result)))
            }

            "NiSkinInstance" | "BSDismemberSkinInstance" => {
                let (i, result) = NiSkinInstance::parse(i)?;
                Ok((i, NifBlock::NiSkinInstance(result)))
            }

            "NiSkinPartition" => {
                let (i, result) = NiSkinPartition::parse(i)?;
                Ok((i, NifBlock::NiSkinPartition(result)))
            }

            "BSShaderTextureSet" => {
                let (i, result) = BSShaderTextureSet::parse(i)?;
                if i.len() > 0 {
                    warn!(
                        "{} bytes left over after parsing BSShaderTextureSet",
                        i.len()
                    );
                }
                Ok((i, NifBlock::BSShaderTextureSet(result)))
            }

            "NiDefaultAVObjectPalette" => {
                let (i, result) = NiDefaultAVObjectPalette::parse(i)?;
                if i.len() > 0 {
                    warn!(
                        "{} bytes left over after parsing NiDefaultAVObjectPalette",
                        i.len()
                    );
                }
                Ok((i, NifBlock::NiDefaultAVObjectPalette(result)))
            }

            "BSXFlags" => {
                let (i, result) = BSXFlags::parse(i)?;
                if i.len() > 0 {
                    warn!("{} bytes left over after parsing BSXFlags", i.len());
                }
                Ok((i, NifBlock::BSXFlags(result)))
            }

            "BSBehaviorGraphExtraData" => {
                let (i, result) = BSBehaviorGraphExtraData::parse(i)?;
                if i.len() > 0 {
                    warn!(
                        "{} bytes left over after parsing BSBehaviorGraphExtraData",
                        i.len()
                    );
                }
                Ok((i, NifBlock::BSBehaviorGraphExtraData(result)))
            }

            "NiControllerManager" => {
                let (i, result) = NiControllerManager::parse(i)?;
                if i.len() > 0 {
                    warn!(
                        "{} bytes left over after parsing NiControllerManager",
                        i.len()
                    );
                }
                Ok((i, NifBlock::NiControllerManager(result)))
            }

            "NiMultiTargetTransformController" => {
                let (i, result) = NiMultiTargetTransformController::parse(i)?;
                if i.len() > 0 {
                    warn!(
                        "{} bytes left over after parsing NiMultiTargetTransformController",
                        i.len()
                    );
                }
                Ok((i, NifBlock::NiMultiTargetTransformController(result)))
            }

            "NiControllerSequence" => {
                let (i, result) = NiControllerSequence::parse(i)?;
                if i.len() > 0 {
                    warn!(
                        "{} bytes left over after parsing NiControllerSequence",
                        i.len()
                    );
                }
                Ok((i, NifBlock::NiControllerSequence(result)))
            }

            "NiFloatInterpolator" => {
                let (i, result) = NiFloatInterpolator::parse(i)?;
                if i.len() > 0 {
                    warn!(
                        "{} bytes left over after parsing NiFloatInterpolator",
                        i.len()
                    );
                }
                Ok((i, NifBlock::NiFloatInterpolator(result)))
            }

            "NiFloatData" => {
                let (i, result) = NiFloatData::parse(i)?;
                //debug!("NiFloatData: {:?}", result);
                if i.len() > 0 {
                    warn!("{} bytes left over after parsing NiFloatData", i.len());
                }
                Ok((i, NifBlock::NiFloatData(result)))
            }

            "BSLightingShaderPropertyFloatController" => {
                let (i, result) = BSLightingShaderPropertyFloatController::parse(i)?;
                if i.len() > 0 {
                    warn!(
                        "{} bytes left over after parsing BSLightingShaderPropertyFloatController",
                        i.len()
                    );
                }
                Ok((i, NifBlock::BSLightingShaderPropertyFloatController(result)))
            }

            "NiBlendFloatInterpolator" => {
                let (i, result) = NiBlendFloatInterpolator::parse(i)?;
                if i.len() > 0 {
                    warn!(
                        "{} bytes left over after parsing NiBlendFloatInterpolator",
                        i.len()
                    );
                }
                Ok((i, NifBlock::NiBlendFloatInterpolator(result)))
            }

            "NiTextKeyExtraData" => {
                let (i, result) = NiTextKeyExtraData::parse(i)?;
                if i.len() > 0 {
                    warn!(
                        "{} bytes left over after parsing NiTextKeyExtraData",
                        i.len()
                    );
                }
                Ok((i, NifBlock::NiTextKeyExtraData(result)))
            }

            "BSLightingShaderProperty" => {
                let (i, result) = BSLightingShaderProperty::parse(i)?;
                if i.len() > 0 {
                    warn!(
                        "{} bytes left over after parsing BSLightingShaderProperty",
                        i.len()
                    );
                }
                Ok((i, NifBlock::BSLightingShaderProperty(result)))
            }

            "BSEffectShaderProperty" => {
                let (i, result) = BSEffectShaderProperty::parse(i)?;
                if i.len() > 0 {
                    warn!(
                        "{} bytes left over after parsing BSEffectShaderProperty",
                        i.len()
                    );
                }
                Ok((i, NifBlock::BSEffectShaderProperty(result)))
            }

            "BSSkin::Instance" => {
                let (i, result) = BSSkinInstance::parse(i)?;
                if i.len() > 0 {
                    warn!("{} bytes left over after parsing BSSkin::Instance", i.len());
                }
                Ok((i, NifBlock::BSSkinInstance(result)))
            }

            "BSSkin::BoneData" => {
                let (i, result) = BSSkinBoneData::parse(i)?;
                if i.len() > 0 {
                    warn!("{} bytes left over after parsing BSSkin::BoneData", i.len());
                }
                Ok((i, NifBlock::BSSkinBoneData(result)))
            }

            "NiAlphaProperty" => {
                let (i, result) = NiAlphaProperty::parse(i)?;
                if i.len() > 0 {
                    warn!("{} bytes left over after parsing NiAlphaProperty", i.len());
                }
                Ok((i, NifBlock::NiAlphaProperty(result)))
            }

            "NiTriShape" => {
                let (i, result) = NiTriShape::parse(i)?;
                Ok((i, NifBlock::NiTriShape(result)))
            }

            "NiTriShapeData" => {
                let (i, result) = NiTriShapeData::parse(i)?;
                Ok((i, NifBlock::NiTriShapeData(result)))
            }

            _ => {
                //debug!("Unhandled block type: {}", block_type);
                Ok((i, NifBlock::Unhandled))
            }
        }
    }

    pub fn as_node(&self) -> Result<&NiNode, String> {
        match self {
            NifBlock::NiNode(node) | NifBlock::BSFadeNode(node) => Ok(node),
            NifBlock::BSMultiBoundNode(block) => Ok(&block.node),
            _ => Err("Block is not a NiNode".to_string()),
        }
    }
}

#[derive(Debug, NomLE)]
pub struct BSBehaviorGraphExtraData {
    pub name: u32,
    pub behaviour_graph_file: u32,
    pub controls_base_skeleton: u8,
}
#[derive(Debug)]
pub struct BSBlastNode {
    // TODO
}
#[derive(Debug)]
pub struct BSBoneLODExtraData {
    // TODO
}
#[derive(Debug)]
pub struct BSBound {
    // TODO
}
#[derive(Debug)]
pub struct BSClothExtraData {
    // TODO
}
#[derive(Debug)]
pub struct BSConnectPointChildren {
    // TODO
}
#[derive(Debug)]
pub struct BSConnectPointParents {
    // TODO
}
#[derive(Debug)]
pub struct BSDamageStage {
    // TODO
}
#[derive(Debug)]
pub struct BSEffectShaderProperty {
    pub parent: NiProperty,
    pub shader_flags_1: Fallout4ShaderPropertyFlags1,
    pub shader_flags_2: Fallout4ShaderPropertyFlags2,
    pub uv_offset: BSVec2,
    pub uv_scale: BSVec2,
    pub source_texture: SizedString32,
    pub texture_clamp_mode: u8,
    pub lighting_influence: u8,
    pub env_map_min_lod: u8,
    pub unused_byte: u8,
    pub falloff_start_angle: f32,
    pub falloff_stop_angle: f32,
    pub falloff_start_opacity: f32,
    pub falloff_stop_opacity: f32,
    pub base_color: BSVec4,
    pub base_color_scale: f32,
    pub soft_falloff_depth: f32,
    pub greyscale_texture: SizedString32,
}

impl Parse<&[u8]> for BSEffectShaderProperty {
    fn parse(i: &[u8]) -> IResult<&[u8], Self> {
        let (i, parent) = NiProperty::parse(i)?;
        let (i, shader_flags_1) = Fallout4ShaderPropertyFlags1::parse(i)?;
        let (i, shader_flags_2) = Fallout4ShaderPropertyFlags2::parse(i)?;
        let (i, uv_offset_x) = le_f32(i)?;
        let (i, uv_offset_y) = le_f32(i)?;
        let uv_offset = BSVec2(glam::Vec2::new(uv_offset_x, uv_offset_y));
        let (i, uv_scale_x) = le_f32(i)?;
        let (i, uv_scale_y) = le_f32(i)?;
        let uv_scale = BSVec2(glam::Vec2::new(uv_scale_x, uv_scale_y));
        let (i, source_texture) = SizedString32::parse(i)?;
        let (i, texture_clamp_mode) = le_u8(i)?;
        let (i, lighting_influence) = le_u8(i)?;
        let (i, env_map_min_lod) = le_u8(i)?;
        let (i, unused_byte) = le_u8(i)?;
        let (i, falloff_start_angle) = le_f32(i)?;
        let (i, falloff_stop_angle) = le_f32(i)?;
        let (i, falloff_start_opacity) = le_f32(i)?;
        let (i, falloff_stop_opacity) = le_f32(i)?;
        let (i, base_color_x) = le_f32(i)?;
        let (i, base_color_y) = le_f32(i)?;
        let (i, base_color_z) = le_f32(i)?;
        let (i, base_color_w) = le_f32(i)?;
        let base_color = BSVec4(glam::Vec4::new(
            base_color_x,
            base_color_y,
            base_color_z,
            base_color_w,
        ));
        let (i, base_color_scale) = le_f32(i)?;
        let (i, soft_falloff_depth) = le_f32(i)?;
        let (i, greyscale_texture) = SizedString32::parse(i)?;
        Ok((
            i,
            Self {
                parent,
                shader_flags_1,
                shader_flags_2,
                uv_offset,
                uv_scale,
                source_texture,
                texture_clamp_mode,
                lighting_influence,
                env_map_min_lod,
                unused_byte,
                falloff_start_angle,
                falloff_stop_angle,
                falloff_start_opacity,
                falloff_stop_opacity,
                base_color,
                base_color_scale,
                soft_falloff_depth,
                greyscale_texture,
            },
        ))
    }
}

#[derive(Debug)]
pub struct BSEffectShaderPropertyColorController {
    // TODO
}

#[derive(Debug)]
pub struct BSEyeCenterExtraData {
    // TODO
}
#[derive(Debug)]
pub struct BSFrustumFOVController {
    // TODO
}
#[derive(Debug)]
pub struct BSFurnitureMarkerNode {
    // TODO
}
#[derive(Debug)]
pub struct BSLagBoneController {
    // TODO
}
#[derive(Debug)]
pub struct BSLeafAnimNode {
    // TODO
}
#[derive(Debug, NomLE)]
pub struct BSLightingShaderProperty {
    /// SSE stores this type-specific field before the inherited NiObjectNET
    /// name (nif.xml `onlyT="BSLightingShaderProperty"`).
    pub shader_type: u32,
    pub ni_shader_property: NiProperty,
    pub shader_flags_1: Fallout4ShaderPropertyFlags1,
    pub shader_flags_2: Fallout4ShaderPropertyFlags2,
    pub uv_offset: BSVec2,
    pub uv_scale: BSVec2,
    pub texture_set: u32,
    pub emissive_color: BSVec3,
    pub emissive_multiple: f32,
    pub texture_clamp_mode: TexClampMode,
    pub alpha: f32,
    pub refraction_strength: f32,
    pub glossiness: f32,
    pub specular_color: BSVec3,
    pub specular_strength: f32,
    pub lighting_effect_1: f32,
    pub lighting_effect_2: f32,
}

#[derive(Debug, NomLE)]
pub struct BSSPWetnessParams {
    pub spec_scale: f32,
    pub spec_power: f32,
    pub min_var: f32,
    pub env_map_scale: f32,
    pub fresnel_power: f32,
    pub metalness: f32,
    pub unknown_1: f32,
}

#[derive(Debug, NomLE)]
pub struct Fallout4ShaderPropertyFlags1 {
    pub raw_flags: u32,
}

impl Fallout4ShaderPropertyFlags1 {
    pub const fn raw(&self) -> u32 {
        self.raw_flags
    }
}

#[derive(Debug, NomLE)]
pub struct Fallout4ShaderPropertyFlags2 {
    pub raw_flags: u32,
}

impl Fallout4ShaderPropertyFlags2 {
    pub const fn raw(&self) -> u32 {
        self.raw_flags
    }
}

#[derive(Debug)]
pub struct BSLightingShaderPropertyColorController {
    // TODO
}
#[derive(Debug, NomLE)]
pub struct BSLightingShaderPropertyFloatController {
    pub parent: NiFloatInterpController,
    pub controlled_variable: LightingShaderControlledFloat,
}
#[derive(Debug)]
pub struct BSMasterParticleSystem {
    // TODO
}
#[derive(Debug)]
pub struct BSMeshLODTriShape {
    // TODO
}
/// Bounding volume of a distant-LOD node. The block is only a reference to the
/// volume data, which is one of `BSMultiBoundAABB` or `BSMultiBoundOBB`.
#[derive(Debug)]
pub struct BSMultiBound {
    pub data: MaxRef,
}

impl Parse<&[u8]> for BSMultiBound {
    fn parse(i: &[u8]) -> IResult<&[u8], Self, nom::error::Error<&[u8]>> {
        let (i, data) = MaxRef::parse(i)?;
        Ok((i, Self { data }))
    }
}

/// Axis-aligned bounds of a `BSMultiBound`: its centre (`position`) and the
/// full per-axis extent, both in the node's local space.
#[derive(Debug, Clone, Copy, NomLE)]
pub struct BSMultiBoundAABB {
    pub position: BSVec3,
    pub extent: BSVec3,
}

/// Skyrim's distant-LOD container node (`meshes/terrain/**` `.btr`/`.bto`).
///
/// It is an `NiNode` with two extra fields, so its children join the scene
/// hierarchy exactly as an `NiNode`'s do.
#[derive(Debug, Clone)]
pub struct BSMultiBoundNode {
    /// The node fields the scene hierarchy consumes.
    pub node: NiNode,
    /// Reference to the `BSMultiBound` block holding this node's volume.
    pub bound: MaxRef,
    /// `SkyrimLayer` culling mode Bethesda appended in version 83. Older
    /// containers end after the bound reference, so the block's recorded size
    /// decides whether it is present.
    pub culling_mode: Option<u32>,
}

impl Parse<&[u8]> for BSMultiBoundNode {
    fn parse(i: &[u8]) -> IResult<&[u8], Self, nom::error::Error<&[u8]>> {
        let (i, node) = NiNode::parse(i)?;
        let (i, bound) = MaxRef::parse(i)?;
        let (i, culling_mode) = if i.len() >= 4 {
            let (i, culling_mode) = le_u32(i)?;
            (i, Some(culling_mode))
        } else {
            (i, None)
        };
        Ok((
            i,
            Self {
                node,
                bound,
                culling_mode,
            },
        ))
    }
}

/// Oriented bounds of a `BSMultiBound`: centre, size and orientation.
#[derive(Debug, Clone, Copy, NomLE)]
pub struct BSMultiBoundOBB {
    pub center: BSVec3,
    pub size: BSVec3,
    pub rotation: BSMatrix3,
}
#[derive(Debug)]
pub struct BSNiAlphaPropertyTestRefController {
    // TODO
}
#[derive(Debug)]
pub struct BSOrderedNode {
    // TODO
}
#[derive(Debug)]
pub struct BSPSysInheritVelocityModifier {
    // TODO
}
#[derive(Debug)]
pub struct BSPSysLODModifier {
    // TODO
}
#[derive(Debug)]
pub struct BSPSysMultiTargetEmitterCtlr {
    // TODO
}
#[derive(Debug)]
pub struct BSPSysRecycleBoundModifier {
    // TODO
}
#[derive(Debug)]
pub struct BSPSysScaleModifier {
    // TODO
}
#[derive(Debug)]
pub struct BSPSysSimpleColorModifier {
    // TODO
}
#[derive(Debug)]
pub struct BSPSysSubTexModifier {
    // TODO
}
#[derive(Debug)]
pub struct BSParentVelocityModifier {
    // TODO
}
#[derive(Debug)]
pub struct BSPositionData {
    // TODO
}
#[derive(Debug)]
pub struct BSProceduralLightningController {
    // TODO
}

#[derive(Debug)]
pub struct BSSkyShaderProperty {
    // TODO
}

#[derive(Debug)]
pub struct BSTreeNode {
    // TODO
}

#[derive(Debug)]
pub struct BSValueNode {
    // TODO
}
#[derive(Debug)]
pub struct BSWaterShaderProperty {
    // TODO
}
#[derive(Debug)]
pub struct BSWindModifier {
    // TODO
}
#[derive(Debug, NomLE)]
pub struct BSXFlags {
    pub ni_extra_data: u32,
    pub integer_data: u32,
}

#[derive(Debug)]
pub struct NiBillboardNode {
    // TODO
}
#[derive(Debug)]
pub struct NiBinaryExtraData {
    // TODO
}
#[derive(Debug)]
pub struct NiBlendBoolInterpolator {
    // TODO
}

#[derive(Debug)]
pub struct NiBlendPoint3Interpolator {
    // TODO
}
#[derive(Debug)]
pub struct NiBoolData {
    // TODO
}
#[derive(Debug)]
pub struct NiBoolInterpolator {
    // TODO
}
#[derive(Debug)]
pub struct NiBoolTimelineInterpolator {
    // TODO
}
#[derive(Debug)]
pub struct NiCamera {
    // TODO
}
#[derive(Debug, NomLE)]
pub struct NiControllerManager {
    pub parent: NiTimeController,
    pub cumulative: Bool,
    #[nom(LengthCount = "le_u32")]
    pub controller_sequences: Vec<u32>,
    pub object_palette: u32,
}

#[derive(Debug, NomLE)]
pub struct NiTimeController {
    pub next_controller: u32,
    pub flags: u16,
    pub frequency: f32,
    pub phase: f32,
    pub start_time: f32,
    pub stop_time: f32,
    pub target: u32,
    //pub unknown_integer: u32
}

#[derive(Debug, NomLE)]
pub struct NiControllerSequence {
    pub parent: NiSequence,
    pub weight: f32,
    pub text_keys: u32,
    pub cycle_type: CycleType,
    pub frequency: f32,
    pub start_time: f32,
    pub stop_time: f32,
    pub manager: u32,
    pub accum_root_name: u32,
    #[nom(LengthCount = "le_u16")]
    pub anim_note_arrays: Vec<u32>,
}

#[derive(Debug, NomLE)]
pub struct NiSequence {
    pub name: u32,
    pub num_controlled_blocks: u32,
    pub array_grow_by: u32,
    #[nom(Count = "num_controlled_blocks")]
    pub controlled_blocks: Vec<ControlledBlock>,
}

#[derive(Debug, NomLE)]
pub struct ControlledBlock {
    pub interpolator: u32,
    pub controller: u32,
    pub priority: u8,
    pub node_name: u32,
    pub property_type: u32,
    pub controller_type: u32,
    pub controller_id: u32,
    pub interpolator_id: u32,
}

#[derive(Debug, NomLE)]
pub struct NiFloatData {
    pub data: KeyGroup,
}

#[derive(Debug)]
pub struct KeyGroup {
    pub num_keys: u32,
    pub key_type: KeyType,
    pub keys: Vec<Key>,
}

impl Parse<&[u8]> for KeyGroup {
    fn parse(i: &[u8]) -> IResult<&[u8], Self, nom::error::Error<&[u8]>> {
        let (i, num_keys) = le_u32(i)?;
        let (i, key_type) = KeyType::parse(i)?;

        let mut data = i;
        let mut keys = Vec::new();

        for _index in 0..num_keys {
            let (i, key) = Key::parse(data, &key_type)?;
            data = i;
            keys.push(key);
        }
        Ok((
            data,
            KeyGroup {
                num_keys,
                key_type,
                keys,
            },
        ))
    }
}

#[derive(Debug)]
pub struct Key {
    pub time: f32,
    pub value: f32,
    pub forward: Option<f32>,
    pub backward: Option<f32>,
    pub tbc: Option<BSVec3>,
}

impl<'a> Key {
    pub fn parse(i: &'a [u8], key_type: &KeyType) -> IResult<&'a [u8], Self> {
        match key_type {
            KeyType::LinearKey | KeyType::ConstKey | KeyType::XyzRotationKey => {
                let (i, time) = le_f32(i)?;
                let (i, value) = le_f32(i)?;
                Ok((
                    i,
                    Key {
                        time,
                        value,
                        forward: None,
                        backward: None,
                        tbc: None,
                    },
                ))
            }
            KeyType::QuadraticKey => {
                let (i, time) = le_f32(i)?;
                let (i, value) = le_f32(i)?;
                let (i, forward) = le_f32(i)?;
                let (i, backward) = le_f32(i)?;
                Ok((
                    i,
                    Key {
                        time,
                        value,
                        forward: Some(forward),
                        backward: Some(backward),
                        tbc: None,
                    },
                ))
            }
            KeyType::TbcKey => {
                let (i, time) = le_f32(i)?;
                let (i, value) = le_f32(i)?;
                let (i, tbc) = BSVec3::parse(i)?;
                Ok((
                    i,
                    Key {
                        time,
                        value,
                        forward: None,
                        backward: None,
                        tbc: Some(tbc),
                    },
                ))
            }
        }
    }
}

#[derive(Debug, NomLE)]
pub struct NiFloatInterpolator {
    pub value: f32,
    pub data: u32,
}
#[derive(Debug)]
pub struct NiLightColorController {
    // TODO
}
#[derive(Debug)]
pub struct NiLightDimmerController {
    // TODO
}
#[derive(Debug)]
pub struct NiLightRadiusController {
    // TODO
}

#[derive(Debug, NomLE)]
pub struct NiMultiTargetTransformController {
    pub parent: NiTimeController,
    #[nom(LengthCount = "le_u16")]
    pub extra_targets: Vec<u32>,
}

#[derive(Debug)]
pub struct NiPSysAgeDeathModifier {
    // TODO
}
#[derive(Debug)]
pub struct NiPSysBombModifier {
    // TODO
}
#[derive(Debug)]
pub struct NiPSysBoundUpdateModifier {
    // TODO
}
#[derive(Debug)]
pub struct NiPSysBoxEmitter {
    // TODO
}
#[derive(Debug)]
pub struct NiPSysColliderManager {
    // TODO
}
#[derive(Debug)]
pub struct NiPSysCylinderEmitter {
    // TODO
}
#[derive(Debug)]
pub struct NiPSysData {
    // TODO
}
#[derive(Debug)]
pub struct NiPSysDragModifier {
    // TODO
}
#[derive(Debug)]
pub struct NiPSysEmitterCtlr {
    // TODO
}
#[derive(Debug)]
pub struct NiPSysEmitterDeclinationCtlr {
    // TODO
}
#[derive(Debug)]
pub struct NiPSysEmitterDeclinationVarCtlr {
    // TODO
}
#[derive(Debug)]
pub struct NiPSysEmitterInitialRadiusCtlr {
    // TODO
}
#[derive(Debug)]
pub struct NiPSysEmitterLifeSpanCtlr {
    // TODO
}
#[derive(Debug)]
pub struct NiPSysEmitterPlanarAngleCtlr {
    // TODO
}
#[derive(Debug)]
pub struct NiPSysEmitterSpeedCtlr {
    // TODO
}
#[derive(Debug)]
pub struct NiPSysGravityModifier {
    // TODO
}
#[derive(Debug)]
pub struct NiPSysGravityStrengthCtlr {
    // TODO
}
#[derive(Debug)]
pub struct NiPSysInitialRotSpeedCtlr {
    // TODO
}
#[derive(Debug)]
pub struct NiPSysInitialRotSpeedVarCtlr {
    // TODO
}
#[derive(Debug)]
pub struct NiPSysMeshEmitter {
    // TODO
}
#[derive(Debug)]
pub struct NiPSysModifierActiveCtlr {
    // TODO
}
#[derive(Debug)]
pub struct NiPSysPlanarCollider {
    // TODO
}
#[derive(Debug)]
pub struct NiPSysPositionModifier {
    // TODO
}
#[derive(Debug)]
pub struct NiPSysRotationModifier {
    // TODO
}
#[derive(Debug)]
pub struct NiPSysSpawnModifier {
    // TODO
}
#[derive(Debug)]
pub struct NiPSysSphereEmitter {
    // TODO
}
#[derive(Debug)]
pub struct NiPSysSphericalCollider {
    // TODO
}
#[derive(Debug)]
pub struct NiPSysUpdateCtlr {
    // TODO
}
#[derive(Debug)]
pub struct NiParticleSystem {
    // TODO
}
#[derive(Debug)]
pub struct NiPathInterpolator {
    // TODO
}
#[derive(Debug)]
pub struct NiPoint3Interpolator {
    // TODO
}
#[derive(Debug)]
pub struct NiPointLight {
    // TODO
}
#[derive(Debug)]
pub struct NiPosData {
    // TODO
}
#[derive(Debug)]
pub struct NiStringExtraData {
    // TODO
}
#[derive(Debug)]
pub struct NiStringsExtraData {
    // TODO
}
#[derive(Debug)]
pub struct NiSwitchNode {
    // TODO
}
#[derive(Debug)]
pub struct NiTransformController {
    // TODO
}
#[derive(Debug)]
pub struct NiTransformData {
    // TODO
}
#[derive(Debug)]
pub struct NiTransformInterpolator {
    // TODO
}
#[derive(Debug)]
pub struct NiVisController {
    // TODO
}
#[derive(Debug)]
pub struct BHKNPCollisionObject {
    // TODO
}
#[derive(Debug)]
pub struct BHKPhysicsSystem {
    // TODO
}
#[derive(Debug)]
pub struct BHKRagdollSystem {
    // TODO
}

#[derive(Debug, NomLE)]
pub struct NiFloatInterpController {
    pub parent: NiSingleInterpController,
}

#[derive(Debug, NomLE)]
pub struct NiSingleInterpController {
    pub parent: NiInterpController,
    pub interpolator: u32,
}

#[derive(Debug, NomLE)]
pub struct NiInterpController {
    pub parent: NiTimeController,
}

#[derive(Debug, NomLE)]
pub struct BSEffectShaderPropertyFloatController {
    pub parent: NiFloatInterpController,
    pub controlled_variable: EffectShaderControlledVariable,
}

#[derive(Debug, NomLE)]
pub struct NiBlendFloatInterpolator {
    pub parent: NiBlendInterpolator,
    pub value: f32,
}

#[derive(Debug, NomLE)]
pub struct NiBlendInterpolator {
    pub flags: u8,
    pub array_size: u8,
    pub weight_threshold: f32,
    #[nom(Cond = "!((flags & 1) != 0)")]
    pub interp_count: Option<u8>,
    #[nom(Cond = "!((flags & 1) != 0)")]
    pub single_index: Option<u8>,
    #[nom(Cond = "!((flags & 1) != 0)")]
    pub high_priority: Option<i8>,
    #[nom(Cond = "!((flags & 1) != 0)")]
    pub next_high_priority: Option<i8>,
    #[nom(Cond = "!((flags & 1) != 0)")]
    pub single_time: Option<f32>,
    #[nom(Cond = "!((flags & 1) != 0)")]
    pub high_weights_sum: Option<f32>,
    #[nom(Cond = "!((flags & 1) != 0)")]
    pub next_high_weights_sum: Option<f32>,
    #[nom(Cond = "!((flags & 1) != 0)")]
    pub high_ease_spinner: Option<f32>,
    #[nom(Cond = "!((flags & 1) != 0)", Count = "array_size")]
    pub interp_array_items: Option<Vec<InterpBlendItem>>,
}

#[derive(Debug, NomLE)]
pub struct InterpBlendItem {
    pub interpolator: u32,
    pub weight: f32,
    pub normalized_weight: f32,
    pub priority: u8,
    pub ease_spinner: f32,
}

#[derive(Debug)]
pub struct NiTextKeyExtraData {
    pub name: u32,
    pub text_keys: Vec<Key>,
}

impl Parse<&[u8]> for NiTextKeyExtraData {
    fn parse(i: &[u8]) -> IResult<&[u8], Self, nom::error::Error<&[u8]>> {
        let (i, name) = le_u32(i)?;
        let (i, key_count) = le_u32(i)?;

        let mut data = i;
        let mut text_keys = Vec::new();

        for _index in 0..key_count {
            let (i, key) = Key::parse(data, &KeyType::LinearKey)?;
            data = i;
            text_keys.push(key);
        }

        Ok((data, NiTextKeyExtraData { name, text_keys }))
    }
}

#[derive(Debug, NomLE)]
pub struct NiExtraData {
    pub string: u32,
}

#[derive(Debug)]
pub struct NiProperty {
    pub parent: NiObjectNET,
}

impl Parse<&[u8]> for NiProperty {
    fn parse(i: &[u8]) -> IResult<&[u8], Self, nom::error::Error<&[u8]>> {
        let (i, name) = le_u32(i)?;
        let (mut i, extra_data_count) = le_u32(i)?;
        if extra_data_count as usize > i.len() / 4 {
            return Err(nom::Err::Failure(nom::error::Error::new(
                i,
                nom::error::ErrorKind::Count,
            )));
        }
        for _ in 0..extra_data_count {
            (i, _) = le_u32(i)?;
        }
        let (i, _) = le_u32(i)?; // controller
        Ok((
            i,
            Self {
                parent: NiObjectNET { name },
            },
        ))
    }
}

#[derive(Debug, NomLE)]
pub struct BSSkinBoneData {
    #[nom(LengthCount = "le_u32")]
    pub bone_list: Vec<BSSkinBoneTrans>,
}

#[derive(Debug, NomLE)]
pub struct BSSkinBoneTrans {
    pub bounding_sphere: Bounds,
    pub rotation: BSMatrix3,
    pub translation: BSVec3,
    pub scale: f32,
}

impl BSSkinBoneTrans {
    pub fn into_matrix4(&self) -> BSMatrix4 {
        // let matrix = BSMatrix4(Mat4 {
        //     self.rotation.0[0], self.rotation.0[1], self.rotation.0[2], 0.0,
        //     self.rotation.0[3], self.rotation.0[4], self.rotation.0[5], 0.0,
        //     self.rotation.0[6], self.rotation.0[7], self.rotation.0[8], 0.0,
        //     self.translation.x, self.translation.y, self.translation.z, 1.0
        // });

        let mut m4 = glam::Mat4::from_mat3(self.rotation.0);
        m4.w_axis = glam::Vec4::new(
            self.translation.0.x,
            self.translation.0.y,
            self.translation.0.z,
            1.0,
        );
        BSMatrix4(m4)
    }
}

#[derive(Debug, NomLE, Clone)]
pub struct BSSkinInstance {
    /// Reference to [NiAVObject] (usually NiNode)
    pub skeleton_root: u32,

    /// Reference to [`BSSkinBoneData`]
    /// Also known as Inverse Bind Matrices
    pub data: u32,

    /// References to `NiNode`
    #[nom(LengthCount = "le_u32")]
    pub bones: Vec<u32>,

    #[nom(LengthCount = "le_u32")]
    pub scales: Vec<BSVec3>,
}

#[derive(Debug, NomLE)]
pub struct NiAlphaProperty {
    pub parent: NiProperty,
    pub flags: AlphaFlags,
    pub threshold: u8,
}

#[derive(Debug, Clone)]
pub struct NiNode {
    pub av: NiAVObject,
    pub children: Vec<u32>,
}

impl Parse<&[u8]> for NiNode {
    fn parse(i: &[u8]) -> IResult<&[u8], Self, nom::error::Error<&[u8]>> {
        // Bethesda's Skyrim streams serialize NiObjectNET on scene nodes as
        // three references (name, extra data, controller), rather than the
        // counted extra-data list used by the generic Gamebryo layout.  Shape
        // blocks use their own NiAVObject serialization and are intentionally
        // parsed through NiAVObject::parse instead.
        let (i, av) = NiAVObject::parse(i)?;
        let (mut i, child_count) = le_u32(i)?;
        if child_count as usize > i.len() / 4 {
            return Err(nom::Err::Failure(nom::error::Error::new(
                i,
                nom::error::ErrorKind::Count,
            )));
        }
        let mut children = Vec::with_capacity(child_count as usize);
        for _ in 0..child_count {
            let (next, child) = le_u32(i)?;
            i = next;
            children.push(child);
        }

        // Skyrim nodes append an effects list.  The renderer does not consume
        // dynamic effects yet, but parsing the list keeps block accounting
        // deterministic and rejects truncated data.
        let (mut i, effect_count) = le_u32(i)?;
        if effect_count as usize > i.len() / 4 {
            return Err(nom::Err::Failure(nom::error::Error::new(
                i,
                nom::error::ErrorKind::Count,
            )));
        }
        for _ in 0..effect_count {
            (i, _) = le_u32(i)?;
        }

        Ok((i, Self { av, children }))
    }
}

impl NiNode {
    pub fn set_translation(&mut self, translation: NifTranslation) {
        self.av.translation = translation;
    }

    pub fn set_rotation(&mut self, rotation: NifRotation) {
        self.av.rotation = rotation;
    }

    pub fn set_scale(&mut self, scale: NifScale) {
        self.av.scale = scale;
    }

    pub fn set_collision_object(&mut self, collision_object: MaxRef) {
        self.av.collision_object = collision_object;
    }

    pub fn set_flags(&mut self, flags: NiAVObjectFlags) {
        self.av.flags = flags;
    }

    pub fn set_children(&mut self, children: Vec<u32>) {
        self.children = children;
    }
}

impl NiAVObjectTraits for NiNode {
    fn flags(&self) -> &NiAVObjectFlags {
        &self.av.flags
    }

    fn translation(&self) -> &NifTranslation {
        &self.av.translation
    }

    fn rotation(&self) -> &NifRotation {
        &self.av.rotation
    }

    fn scale(&self) -> &NifScale {
        &self.av.scale
    }

    fn collision_object(&self) -> &MaxRef {
        &self.av.collision_object
    }
}

impl NiObjectNETTraits for NiNode {
    fn name(&self) -> u32 {
        self.av.object.name
    }
}

#[derive(Debug, NomLE, Clone)]
pub struct NiObjectNET {
    pub name: u32,
}

pub trait NiObjectNETTraits {
    fn name(&self) -> u32;
}

#[derive(Debug, NomLE)]
pub struct NiDefaultAVObjectPalette {
    pub scene: u32,
    #[nom(LengthCount = "le_u32")]
    pub objs: Vec<NameIndex>,
}

#[derive(Debug, NomLE)]
pub struct NameIndex {
    pub name: SizedString32,
    pub index: u32,
}

#[derive(Debug, Clone)]
pub struct NiAVObject {
    pub object: NiObjectNET,
    pub flags: NiAVObjectFlags,
    pub translation: NifTranslation,
    pub rotation: NifRotation,
    pub scale: NifScale,
    pub collision_object: MaxRef,
}

impl Parse<&[u8]> for NiAVObject {
    fn parse(i: &[u8]) -> IResult<&[u8], Self, nom::error::Error<&[u8]>> {
        if let Ok((rest, av)) = parse_compact_av_object(i) {
            if valid_av_transform(&av) {
                return Ok((rest, av));
            }
        }
        parse_counted_av_object(i)
    }
}

fn parse_compact_av_object(i: &[u8]) -> IResult<&[u8], NiAVObject, nom::error::Error<&[u8]>> {
    let (i, name) = le_u32(i)?;
    let (i, _) = le_u32(i)?; // extra data
    let (i, _) = le_u32(i)?; // controller
    let (i, flags) = NiAVObjectFlags::parse(i)?;
    let (i, translation) = NifTranslation::parse(i)?;
    let (i, rotation) = NifRotation::parse(i)?;
    let (i, scale) = NifScale::parse(i)?;
    let (i, collision_object) = MaxRef::parse(i)?;

    Ok((
        i,
        NiAVObject {
            object: NiObjectNET { name },
            flags,
            translation,
            rotation,
            scale,
            collision_object,
        },
    ))
}

fn parse_counted_av_object(i: &[u8]) -> IResult<&[u8], NiAVObject, nom::error::Error<&[u8]>> {
    let (i, name) = le_u32(i)?;
    let (mut i, extra_data_count) = le_u32(i)?;
    if extra_data_count as usize > i.len() / 4 {
        return Err(nom::Err::Failure(nom::error::Error::new(
            i,
            nom::error::ErrorKind::Count,
        )));
    }
    for _ in 0..extra_data_count {
        (i, _) = le_u32(i)?;
    }
    let (i, _) = le_u32(i)?; // controller
    let (i, flags) = NiAVObjectFlags::parse(i)?;
    let (i, translation) = NifTranslation::parse(i)?;
    let (i, rotation) = NifRotation::parse(i)?;
    let (i, scale) = NifScale::parse(i)?;
    let (i, collision_object) = MaxRef::parse(i)?;
    let av = NiAVObject {
        object: NiObjectNET { name },
        flags,
        translation,
        rotation,
        scale,
        collision_object,
    };
    if !valid_av_transform(&av) {
        return Err(nom::Err::Failure(nom::error::Error::new(
            i,
            nom::error::ErrorKind::Verify,
        )));
    }
    Ok((i, av))
}

fn valid_av_transform(av: &NiAVObject) -> bool {
    let rotation = av.rotation.0 .0;
    av.translation.0 .0.is_finite()
        && rotation.is_finite()
        && av.scale.0.is_finite()
        && av.scale.0 != 0.0
        && rotation.determinant().is_finite()
        && rotation.determinant().abs() > 1.0e-4
}

pub trait NiAVObjectTraits {
    fn flags(&self) -> &NiAVObjectFlags;
    fn translation(&self) -> &NifTranslation;
    fn rotation(&self) -> &NifRotation;
    fn scale(&self) -> &NifScale;
    fn collision_object(&self) -> &MaxRef;
}

impl NiObjectNETTraits for NiAVObject {
    fn name(&self) -> u32 {
        self.object.name
    }
}

#[derive(Debug, Clone)]
pub struct NiAVObjectFlags(u32);

impl NiAVObjectFlags {
    pub fn hidden(&self) -> bool {
        self.0 & 0x0001 != 0
    }

    pub fn mesh_collision(&self) -> bool {
        self.0 & 0x0002 != 0
    }

    pub fn box_collision(&self) -> bool {
        self.0 & 0x0004 != 0
    }

    pub fn has_collision(&self) -> bool {
        self.0 & 0x0020 != 0
    }
}

impl Parse<&[u8]> for NiAVObjectFlags {
    fn parse(i: &[u8]) -> IResult<&[u8], Self, nom::error::Error<&[u8]>> {
        let (i, raw) = le_u32(i)?;

        Ok((i, NiAVObjectFlags(raw)))
    }
}

#[cfg(test)]
mod material_property_tests {
    use super::*;

    fn push_property(bytes: &mut Vec<u8>, name: u32, extras: &[u32], controller: u32) {
        bytes.extend_from_slice(&name.to_le_bytes());
        bytes.extend_from_slice(&(extras.len() as u32).to_le_bytes());
        for extra in extras {
            bytes.extend_from_slice(&extra.to_le_bytes());
        }
        bytes.extend_from_slice(&controller.to_le_bytes());
    }

    fn push_f32(bytes: &mut Vec<u8>, values: &[f32]) {
        for value in values {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
    }

    fn push_string(bytes: &mut Vec<u8>, value: &str) {
        bytes.extend_from_slice(&(value.len() as u32).to_le_bytes());
        bytes.extend_from_slice(value.as_bytes());
    }

    #[test]
    fn lighting_property_consumes_the_complete_inherited_property() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0u32.to_le_bytes());
        push_property(&mut bytes, u32::MAX, &[], u32::MAX);
        bytes.extend_from_slice(&0x8240_0302u32.to_le_bytes());
        bytes.extend_from_slice(&0x0000_8001u32.to_le_bytes());
        push_f32(&mut bytes, &[0.0, 0.0, 1.0, 1.0]);
        bytes.extend_from_slice(&92u32.to_le_bytes());
        push_f32(&mut bytes, &[0.0, 0.0, 0.0, 1.0]);
        bytes.extend_from_slice(&3u32.to_le_bytes());
        push_f32(&mut bytes, &[1.0, 0.0, 80.0, 1.0, 1.0, 1.0, 1.0, 0.3, 2.0]);

        let (remaining, property) = BSLightingShaderProperty::parse(&bytes).unwrap();
        assert!(remaining.is_empty());
        assert_eq!(property.texture_set, 92);
        assert_eq!(property.shader_flags_1.raw(), 0x8240_0302);
        assert_eq!(property.alpha, 1.0);
        assert_eq!(property.glossiness, 80.0);
    }

    #[test]
    fn effect_property_reads_inline_texture_paths() {
        let mut bytes = Vec::new();
        push_property(&mut bytes, u32::MAX, &[], 20);
        bytes.extend_from_slice(&0x8000_0078u32.to_le_bytes());
        bytes.extend_from_slice(&49u32.to_le_bytes());
        push_f32(&mut bytes, &[1.0, 8.3, 4.0, 2.0]);
        push_string(&mut bytes, r"textures\effects\a.dds");
        bytes.extend_from_slice(&[3, 4, 5, 0]);
        push_f32(&mut bytes, &[0.1, 0.2, 0.3, 0.4]);
        push_f32(&mut bytes, &[1.0, 0.5, 0.25, 0.75, 2.0, 0.0]);
        push_string(&mut bytes, r"textures\effects\mask.dds");

        let (remaining, property) = BSEffectShaderProperty::parse(&bytes).unwrap();
        assert!(remaining.is_empty());
        assert_eq!(property.parent.parent.name, u32::MAX);
        assert_eq!(property.source_texture.0, r"textures\effects\a.dds");
        assert_eq!(property.greyscale_texture.0, r"textures\effects\mask.dds");
        assert_eq!(property.base_color.0.w, 0.75);
    }

    #[test]
    fn alpha_property_reads_flags_after_its_inherited_property() {
        let mut bytes = Vec::new();
        push_property(&mut bytes, 3, &[8], u32::MAX);
        bytes.extend_from_slice(&0x0201u16.to_le_bytes());
        bytes.push(128);
        let (remaining, property) = NiAlphaProperty::parse(&bytes).unwrap();
        assert!(remaining.is_empty());
        assert!(property.flags.blend_enabled());
        assert!(property.flags.test_enabled());
        assert_eq!(property.threshold, 128);
    }
}
