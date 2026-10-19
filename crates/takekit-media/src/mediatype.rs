//! Building and reading Media Foundation media types.
//!
//! A media type is a bag of attributes, and two of them — frame size and frame
//! rate — are a pair of 32-bit numbers packed into one 64-bit value. The
//! packing is the only thing here that is easy to get wrong.

use windows::Win32::Media::MediaFoundation::{
    IMFMediaType, MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE,
    MF_MT_AUDIO_BLOCK_ALIGNMENT, MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MFAudioFormat_PCM, MFCreateMediaType,
    MFMediaType_Audio, MFMediaType_Video, MFVideoFormat_NV12, MFVideoInterlace_Progressive,
};
use windows::core::GUID;

use crate::error::Result;

/// Two 32-bit values in one 64-bit attribute, the way Media Foundation stores a
/// size or a ratio.
pub fn pack(high: u32, low: u32) -> u64 {
    (u64::from(high) << 32) | u64::from(low)
}

/// The halves of a packed attribute.
pub fn unpack(value: u64) -> (u32, u32) {
    ((value >> 32) as u32, value as u32)
}

/// An uncompressed video type: `subtype` at `width`×`height`, `fps` frames a
/// second, progressive, square pixels.
pub fn video_type(subtype: GUID, width: u32, height: u32, fps: u32) -> Result<IMFMediaType> {
    // SAFETY: `SetGUID`/`SetUINT64`/`SetUINT32` write scalars into an object
    // this function owns and returns.
    unsafe {
        let media_type = MFCreateMediaType()?;
        media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        media_type.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
        media_type.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
        media_type.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
        media_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        media_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        Ok(media_type)
    }
}

/// The video type a camera is asked for: NV12, its own resolution, the take's
/// frame rate.
///
/// NV12 rather than RGB because it is what a camera produces and what an
/// encoder wants — asking for RGB would make Media Foundation decode and
/// convert every frame, and then hand it over bottom-up.
pub fn camera_type(width: u32, height: u32, fps: u32) -> Result<IMFMediaType> {
    video_type(MFVideoFormat_NV12, width, height, fps)
}

/// 16-bit PCM, the one uncompressed audio format every Windows capture path can
/// produce without a conversion.
pub fn pcm_type(sample_rate: u32, channels: u32) -> Result<IMFMediaType> {
    let block_alignment = channels * 2;
    let bytes_per_second = sample_rate * block_alignment;
    // SAFETY: scalar attribute writes on an owned object, as above.
    unsafe {
        let media_type = MFCreateMediaType()?;
        media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        media_type.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
        media_type.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels)?;
        media_type.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, sample_rate)?;
        media_type.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        media_type.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, block_alignment)?;
        media_type.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, bytes_per_second)?;
        Ok(media_type)
    }
}

/// The frame size of a media type, which is how a capture learns what the
/// device it just opened is going to hand over.
pub fn frame_size(media_type: &IMFMediaType) -> Result<(u32, u32)> {
    // SAFETY: reads one packed 64-bit attribute into a local.
    let packed = unsafe { media_type.GetUINT64(&MF_MT_FRAME_SIZE)? };
    Ok(unpack(packed))
}

/// The frame rate of a media type, as a fraction.
pub fn frame_rate(media_type: &IMFMediaType) -> Result<(u32, u32)> {
    // SAFETY: reads one packed 64-bit attribute into a local.
    let packed = unsafe { media_type.GetUINT64(&MF_MT_FRAME_RATE)? };
    let (numerator, denominator) = unpack(packed);
    Ok((numerator.max(1), denominator.max(1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_packed_attribute_round_trips() {
        assert_eq!(unpack(pack(1920, 1080)), (1920, 1080));
        assert_eq!(unpack(pack(30000, 1001)), (30000, 1001));
        assert_eq!(unpack(pack(0, 7)), (0, 7));
    }

    /// A 48 kHz stereo PCM type has to describe itself consistently: the block
    /// alignment and the byte rate follow from the channels and the sample rate,
    /// and an encoder that disagrees produces a file that plays at the wrong
    /// speed.
    #[test]
    fn a_pcm_type_describes_its_own_throughput() {
        let media_type = pcm_type(48_000, 2).expect("a media type");
        assert_eq!(
            unsafe { media_type.GetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT) }.unwrap(),
            4
        );
        assert_eq!(
            unsafe { media_type.GetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND) }.unwrap(),
            192_000
        );
    }

    /// The frame size an open device reports is what the writer has to size its
    /// frames by, so it has to survive the round trip through the attribute bag.
    #[test]
    fn a_video_type_reports_the_size_it_was_built_with() {
        let media_type = camera_type(1280, 720, 30).expect("a media type");
        assert_eq!(frame_size(&media_type).unwrap(), (1280, 720));
        assert_eq!(frame_rate(&media_type).unwrap(), (30, 1));
    }
}
