function id<T>(x: T): T { return x }
class G<T extends object = {}> { m<U>(u: U): T | U { return u as T | U } }
const k = id<string>("a");
new Map<string, number>();
const h = id<number>;
