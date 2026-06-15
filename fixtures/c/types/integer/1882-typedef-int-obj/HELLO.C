typedef int my_int;
typedef my_int alias_int;
int main(void) {
  alias_int x = 42;
  my_int y = x + 1;
  return y;
}
