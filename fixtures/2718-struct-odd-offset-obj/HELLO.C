struct S { char tag; int value; };
struct S s;
int main(void) {
  s.tag = 'A';
  s.value = 1000;
  return s.value;
}
