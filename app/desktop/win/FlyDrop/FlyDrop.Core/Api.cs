﻿using FlyDrop.Core.Internal;
using FlyDrop.Core.Models.Commands;
using FlyDrop.Core.Models.Common;
using FlyDrop.Core.Models.Events;
using FlyDrop.Core.Models.Queries;
using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Linq;
using System.Text;
using System.Text.Json;
using System.Text.Json.Serialization;
using System.Threading.Channels;
using System.Threading.Tasks;

namespace FlyDrop.Core
{
    public delegate void EventCallback(ApiEvent ev);


    public class Api : IDisposable
    {
        public bool IsInitalized { get; private set; }
        private readonly Mutex _mutex;
        private readonly JsonSerializerOptions _options;
       

        public Api() : this(null)
        {
     
        }

        public Api(JsonSerializerOptions? options = null)
        {
            var id = Process.GetCurrentProcess().Id;
            _mutex = new Mutex(true, $"flydrop-{id}");
            _options = options ?? new JsonSerializerOptions(JsonSerializerOptions.Default);
            _options.PropertyNamingPolicy = new JsonSnakeCaseLowerNamingPolicy();
            IsInitalized = false;
        }

        public async Task<string> InitializeAsync(string directory, Func<ApiEvent, Task> eventCallback)
        {
            var channel = Channel.CreateBounded<string>(1);

            Native.Initialize(directory, (ev) =>
            {
                try
                {
                    var e = JsonSerializer.Deserialize<ApiEvent>(ev, _options);
                    if (e != null)
                    {
                        //  _ = Task.Run(() => { eventCallback(e); });
                        _ = eventCallback(e);

                    }
                }
                catch (Exception ex)
                {
                    Console.Error.WriteLine("Failed to read event from core: {0}", ev);
                    Console.Error.WriteLine("{0}", ex);
                }
            }, (res) =>
            {
                channel.Writer.WriteAsync(res);
            });

            var r = await channel.Reader.ReadAsync();
            IsInitalized = true;
            return r;
        }

        public static async Task<Api> CreateAsync(string directory, EventCallback eventCallback, JsonSerializerOptions? options = null)
        {
            var api = new Api(options);
            var channel = Channel.CreateBounded<object>(1);

            Native.Initialize(directory,(ev) =>
            {
                // Console.WriteLine(ev);
                try
                {
                    var e = JsonSerializer.Deserialize<ApiEvent>(ev, api._options);
                    if(e != null)
                    {
                        _ = Task.Run(() => { eventCallback(e); });
                    }
                }
                catch (Exception ex)
                {
                    Console.Error.WriteLine("Failed to read event from core: {0}", ev);
                    Console.Error.WriteLine("{0}", ex);
                }
            }, (_) =>
            {
                channel.Writer.WriteAsync(new object());
            });

            await channel.Reader.ReadAsync();
            api.IsInitalized = true;
            return api;

        }

        public static Api Create(string directory, EventCallback eventCallback, JsonSerializerOptions? options = null)
        {
            var api = new Api(options);
            var channel = new SemaphoreSlim(0, 1);
            try
            {
                Native.Initialize(directory, (ev) =>
                {
                    // Console.WriteLine(ev);
                    try
                    {
                        var e = JsonSerializer.Deserialize<ApiEvent>(ev, api._options);
                        if (e != null)
                        {
                            _ = Task.Run(() => { eventCallback(e); });
                        }
                    }
                    catch (Exception ex)
                    {
                        Console.Error.WriteLine("Failed to read event from core: {0}", ev);
                        Console.Error.WriteLine("{0}", ex);
                    }
                }, (_) =>
                {
                    Console.WriteLine("cool");
                    channel.Release();
                });
            }
            catch(Exception e) 
            {
                Console.Error.WriteLine("Failed to init Api: {0}", e);
            }
            

            channel.Wait();
            api.IsInitalized = true;
            return api;

        }
        /*
           public ValueTask<string> CommandAsync(string json)
                {
                    var channel = Channel.CreateBounded<string>(1);
                    Native.Cmd(json, (res) =>
                    {
                        channel.Writer.WriteAsync(res);
                    });
                    return channel.Reader.ReadAsync();
                }*/
        public ValueTask<ApiResponse<TRes>> QueryAsync<TReq, TRes>(TReq request) where TReq : QueryRequest where TRes : QueryResponse
        {
            var channel = Channel.CreateBounded<ApiResponse<TRes>>(1);
            var json = JsonSerializer.Serialize(request, _options);

            Native.Query(json, (res) =>
            {
                try
                {
                    var response = JsonSerializer.Deserialize<ApiResponse<TRes>>(res, _options);
                    channel.Writer.WriteAsync(response);
                }
                catch (Exception ex)
                {
                    Console.Error.WriteLine("Failed to read query response from core: {0}", res);
                    channel.Writer.Complete(ex);
                }
            });
            return channel.Reader.ReadAsync();
        }

        public ValueTask<ApiResponse<TRes>> CommandAsync<TReq, TRes>(TReq request) where TReq : CommandRequest where TRes : CommandResponse
        {
            var channel = Channel.CreateBounded<ApiResponse<TRes>>(1);
            var json = JsonSerializer.Serialize(request, _options);

            Native.Cmd(json, (res) =>
            {
                try
                {
                    var response = JsonSerializer.Deserialize<ApiResponse<TRes>>(res, _options);
                    channel.Writer.WriteAsync(response);
                }
                catch (Exception ex)
                {
                    Console.Error.WriteLine("Failed to read cmd response from core: {0}", res);
                    channel.Writer.Complete(ex);

                }
            });
            return channel.Reader.ReadAsync();
        }

        public void Dispose()
        {
            _mutex?.Dispose();
        }
    }
}
