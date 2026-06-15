struct Inner { int vals[3]; };
struct Outer { int tag; struct Inner in; };
int main()
{
  struct Outer o;

  o.tag = 100;
  o.in.vals[0] = 1;
  o.in.vals[1] = 2;
  o.in.vals[2] = 4;
  return o.tag + o.in.vals[0] + o.in.vals[2];
}
