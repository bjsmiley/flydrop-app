/*// <auto-generated>
// This code is generated by csbindgen.
// DON'T CHANGE THIS DIRECTLY.
// </auto-generated>
#pragma warning disable CS8500
#pragma warning disable CS8981
using System;
using System.Runtime.InteropServices;

namespace CsBindgen
{
    internal static unsafe partial class NativeG
    {
        const string __DllName = "libfd";

        [DllImport(__DllName, EntryPoint = "listen", CallingConvention = CallingConvention.Cdecl, ExactSpelling = true)]
        public static extern void listen(delegate* unmanaged[Cdecl]<Buffer*, void> on_event);


    }

    [StructLayout(LayoutKind.Sequential)]
    internal unsafe partial struct Buffer
    {
        public byte* ptr;
        public int len;
        public int cap;
    }



}
    */