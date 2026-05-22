int target = 42;
int *alias = &target;
int main(void) {
  return *alias;
}
