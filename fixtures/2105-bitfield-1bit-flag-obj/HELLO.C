struct Flags { unsigned f1 : 1; unsigned f2 : 1; unsigned val : 14; };
int main(void) {
  struct Flags fl;
  fl.f1 = 1;
  fl.f2 = 0;
  fl.val = 1000;
  return (int)fl.f1 * 100 + (int)fl.val;
}
